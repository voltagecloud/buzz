# Relay data layer: port specification

This is the behavior spec for the Rust port of buzz-app's TypeScript relay data layer
(`src/features/relay` and its neighbors) and the Tauri host's signing and validation
code. It records what the reference app does, including behavior that looks wrong.
Where the reference behavior looks like a bug, the spec says so, and §9.3 lists every
such decision for the port.

**Sources and citation convention**

| Prefix | Meaning | Revision |
|---|---|---|
| `src/…`, `src-tauri/…`, `docs/…`, `crates/…` (no prefix) | buzz-app checkout, `/home/ben/projects/buzz-app` | `395f6e3` (2026-10-09) |
| `buzz:crates/…`, `buzz:docs/…` | relay server checkout `block/buzz`, `/tmp/research/buzz` | `5142cf6` (2026-10-09) |

Citations are `path:line` or `path:start-end`. Line numbers are for these revisions.
All pubkeys, event ids and hashes are 64-char lowercase hex. Signatures are 128-char hex.
Channel ids (`h` tag values) are lowercase hyphenated UUIDs. Times are Unix seconds
unless a name ends in `Ms`. "Relay author" or `relayAuthor` means the community's
NIP-11 `self` pubkey (§1.1).

**Contents**

0. Quick reference: architecture facts, event kinds
1. Community model
2. Identity
3. Connection lifecycle (WebSocket)
4. HTTP reads (`POST /query`, read scheduler)
5. Feature data flows: 5a sidebar, 5b timeline, 5c threads, 5d sending, 5e DMs,
   5f profiles, 5g members, 5h presence and typing, 5i read state and unread, 5j user status,
   5k custom emoji, 5l notifications and inbox, 5m channel lifecycle, 5n media, 5o other kinds
6. Outbox and durability
7. Local persistence inventory and SQLite schema
8. Event validation and access revocation (including the Tauri host command surface)
9. Port plan: Rust module breakdown, risky behaviors, decisions

---

## 0. Quick reference

### 0.1 Architecture facts that shape the port

1. **One community = one HTTPS origin** (`https://host[:port]`). The per-device partition key is
   `"<origin>:<viewerHex>"` (§1.1). Never change this byte format (§9.2 #16).
2. **Reads and writes are HTTP. The WebSocket carries only live traffic.** In the packaged app:
   - Finite reads are `POST /query`.
   - Writes are `POST /events`.
   - Community admin uses `/api/...` routes.
   - Every one of these requests is NIP-98 signed with a fresh kind-27235 event.
   The single WebSocket per community carries NIP-42 AUTH, live REQ subscriptions and
   presence (kind 20001) publication. There is no HTTP fallback for uncertain writes,
   and no automatic replay (§3.0, §3.7).
3. **The relay is an authority with its own key.** NIP-11 `self` (not `pubkey`) signs channel
   metadata, rosters, thread summaries, window bounds, membership notices, presence snapshots
   and the community roster. Only accept those kinds when `pubkey == self` (§8.2).
4. **Every inbound event is fully verified** (id hash and Schnorr signature) before use. One bad
   event rejects the whole HTTP response or fails the whole live route (§8.1). In the reference
   app this runs in TypeScript. **The Rust port must do it in Rust** (§8.7.1).
5. **The relay paginates with signed bound events, not envelope fields.**
   - `/query` always returns a flat JSON array of events.
   - Channel windows carry a relay-signed 39006 bound.
   - Thread windows carry a relay-signed 39007 bound, keyed by a SHA-256 binding.
   - Cursors are `(until, before_id)` pairs echoed verbatim (§4.2, §5b, §5c).
6. **Durable outbox.**
   - The unsigned intent (with its final id) is persisted before signing.
   - The signed bytes are persisted before publishing.
   - A retry republishes the same bytes. Events are never re-signed or re-dated.
   - Restored operations come back as `unknown` and are never published automatically.
   - Outcomes are `sending | accepted | unknown | failed | seen` (§6).
7. **Read state** is NIP-44 self-encrypted kind 30078 with `d=read-state:<32hex slot>` and one slot
   per device. Merge is a per-key `max` across slots. Unread counts are computed on the client
   from sampled evidence. There is no server count API (§5i).
8. **Everything has a budget**: reader 3 slots with 1 background, a 10 s deadline counted from
   enqueue, 1–4 filters and limit ≤500 per read; HTTP lane 6 slots; live batches of 10 channels;
   windows 2400 rows / 8 MiB; and so on. The budgets are part of the contract.
   `docs/relay-queries.md` is the maintained authority for them.

### 0.2 Event kinds used by the client

W = the app writes it. R = the app reads it. Relay = signed by the relay's NIP-11 `self`, and
accepted only from that key.

| Kind | Meaning | Author | W/R | Spec |
|---|---|---|---|---|
| 0 | profile metadata (NIP-01, NIP-OA `auth` tag for agents) | user | W/R | §5f |
| 5 | deletion (NIP-09). The relay requires **exactly one** `e`/`a` target | user | W/R | §5d.8 |
| 7 | reaction (`h`, `e`, optional `emoji`; no `p`/`k`) | user | W/R | §5d.9 |
| 9 | channel chat message (NIP-29) | user, or relay for workflow output | W/R | §5d |
| 1984 | report (NIP-56) | user | W | §5d.10 |
| 8000 / 8001 | NIP-43 community member added / removed | relay | – | §1.6, §5g.5 |
| 9000 / 9001 | NIP-29 put-user / remove-user (channel members) | user | W | §5g |
| 9002 | NIP-29 edit channel metadata (also archive/unarchive) | user | W | §5m |
| 9005 | NIP-29 delete-event (moderator) | user | R only | §5b.5 |
| 9007 | NIP-29 create channel (client-chosen UUID `h`) | user | W | §5m.1 |
| 9008 | NIP-29 delete channel | user | W | §5m.2 |
| 9021 / 9022 | NIP-29 join / leave channel | user | W | §5m.2 |
| 9030 / 9031 / 9032 | community member add / remove / role | admin | W | §1.9 |
| 9033 | set community profile/icon (admin) | admin | – | §1.5 |
| 9035 / 9036 | identity archive / unarchive (NIP-IA) | user | W | §8.6 |
| 10100 | agent public metadata (`channel_add_policy`, …) | agent | R | §5f.2 |
| 13534 | NIP-43 community membership list (`["member",pk,role]`) | relay | R | §1.9, §5g.5 |
| 13535 | identity-archive snapshot | relay | R | §8.6 |
| 20001 | presence (content `online\|away\|offline`). Published over WS; HTTP snapshot is relay-signed | user / relay | W/R | §5h |
| 20002 | typing (`h` plus optional NIP-10 `e`) | agents | R only | §5h.6 |
| 22242 | NIP-42 AUTH | user | W | §2.6 |
| 24242 | Blossom media auth | user (host only) | W | §2.8 |
| 27235 | NIP-98 HTTP auth | user (host only) | W | §2.7 |
| 28936 | NIP-43 leave community (`[["-"]]`) | user | W | §1.8 |
| 30030 | custom emoji set `d=buzz:custom-emoji` | user | W/R | §5k |
| 30078 | app data: read state, sidebar prefs, channel/Me kits (NIP-44 self-encrypted) | user | W/R | §5a.6, §5i |
| 30175 / 30178 / 30177 | agent/team catalog, managed-agent registration (out of scope) | user | W/R | — |
| 30300 | reminders (NIP-44 self-encrypted) | user | W/R | §5o |
| 30315 | user status `d=general` (NIP-38) | user | W/R | §5j |
| 30622 | per-viewer hidden-DM set (NIP-DV) | relay | R | §5e.5 |
| 39000 / 39001 / 39002 | channel metadata / admins / roster (`d`=channel UUID) | relay | R | §5a, §5g |
| 39005 | thread summary | relay | R | §5c.2 |
| 39006 | channel window bound | relay | R | §5b.1 |
| 39007 | thread window bound | relay | R | §5c.4 |
| 40002 | agent message envelope (content JSON `{content}`) | agent | R | §5b |
| 40003 | message edit | user | W/R | §5d.7 |
| 40008 | diff message | agent | R | §5b.5 |
| 40099 | system message (membership activity row) | relay | R | §5g.4 |
| 40100 | canvas (templates feature) | user | W/R | — |
| 41010 | open DM (command; the reply carries `channel_id`) | user | W | §5e.1 |
| 41011 | expand DM into a new DM (the app never sends it) | user | – | §5e.4 |
| 41012 | hide DM | user | W | §5e.5 |
| 42000 | product feedback | user | W | — |
| 44100 / 44101 | member added / removed notification (a hint only) | relay | R | §5a.2 |
| 45001 / 45003 | forum post / forum comment (activity kinds) | user | R | §5a.4 |
| 45010 / 45011 | artifacts (out of scope) | | | — |
| 24200 / 44200 | agent observer / metrics (out of scope) | | | — |
| 30620 / 46020 | workflow def / trigger (out of scope) | | | — |

`CHANNEL_KINDS` for the live `#h` subscription is
`[9,40002,40008,45001,45003,40099,40100,40003,5,9005,7,39000,39002,39005,20002]` (`src/features/relay/live.ts:200-203`).
`CHANNEL_ROW_KINDS` (timeline rows) is `[9,40002,40008,40099]` (`src/features/relay/membership.ts:6-8`).

---

## 1. Community model

### 1.1 What a community is
- A community is a **canonical HTTPS origin** (`https://host[:port]`, no path). The origin is the community ID and the partition key for every per-community store. Source: `src/features/communities/destination.ts:1-23`, `docs/communities.md:25-31`.
- Per-device state is keyed by `relayPartition(origin, viewer) = "${origin}:${viewer}"`, where viewer is the hex pubkey. Source: `src/features/relay/partition.ts:4-6`.
- The relay identity is the NIP-11 **`self`** field (64 lowercase hex). It is **not** NIP-11 `pubkey`, which is the operator contact. If `self` is missing, connecting fails with `"Relay did not advertise its identity"`. Source: `src/features/relay/native.ts:324-333`, `docs/identity.md:206-207`, `buzz:crates/buzz-relay/src/nip11.rs:76-77`.

### 1.2 URL normalization (`relayOrigin`), `destination.ts:2-23`
Input rules:
- Trim whitespace. Maximum length is 2048.
- The input must match `/^(?:wss|https):\/\/[^/?#\\\s@]+\/?$/i`.
- Rejected inputs: `ws://`, `http://`, credentials (`@`), any path other than one root `/`, a query, a fragment. Error text: "Enter a wss:// or https:// relay URL without credentials, a path, query, or fragment."

Normalization:
- Parse with WHATWG URL, then force the scheme to `https:`.
- Strip one trailing DNS dot from the hostname.
- Return `url.origin`. This lowercases the host, drops a default `:443`, and keeps non-default ports.

Examples: `wss://Relay.Example.com./` → `https://relay.example.com`. `wss://h:8443` → `https://h:8443`.

Rust check: `relay::origin()` (`src-tauri/src/relay.rs:58-72`) requires `https`, a host, no userinfo, path `/`, no query, no fragment, and length ≤ 2048. Rust receives the origin string, and `Url::parse` adds the trailing `/`.

WS URL for live traffic: `httpOrigin.replace(/^http/, "ws")` → `wss://host[:port]`, with no trailing slash (`src/features/relay/transport.ts:1183`).

`communityDestination(value)` returns `{id, url, name: new URL(url).host}` (`destination.ts:65-73`).
- Optional legacy aliases come from build env `VITE_BUZZ_COMMUNITY_ALIASES`: a JSON object mapping `/^[a-z][a-z0-9-]{0,63}$/` aliases to origins. No aliases ship by default, so a port can skip this.
- If no alias applies, `id == url`.

### 1.3 Saved communities (persistence)
`localStorage["buzz-client.v1:<viewerHex>"]` holds (`src/features/communities/service.ts:12-18,101-109,180-238`):
```json
{ "profile": {"name":"","picture":"","about":""},
  "memberships": [{"id":"https://relay.example.com","name":"Relay Name","icon":"https://..."}],
  "selected": "https://relay.example.com" }
```
- `selected: null` means "Personal space", where no community is open.
- On load, every membership ID is re-normalized through `communityDestination` and deduplicated by ID. Entries that fail normalization are dropped, except unresolved aliases, which are kept but hidden.
- `icon` is kept only if it starts with `https://`.
- If `selected` is not among the memberships, it becomes `null`.

Startup acquires **only the selected** community's session (`service.ts:261`). Other communities are opened lazily when the user selects them. Sessions are never evicted; there is no session cap.

`joined(membership, profile)`:
- Upserts by ID and selects the new community.
- Seeds the local default profile only if it is empty.
- The native path requires the save to succeed and throws "Could not save this community on this device." otherwise (`service.ts:297-317`).

Display name on join is the NIP-11 `name`, unless it equals `"Buzz Relay"`, in which case the host is used. `icon` is set from NIP-11 only if it is `https://` (`src/features/communities/CommunityDialog.tsx:229-238`).

Join recovery journal: `localStorage["buzz-community-joins.v1:<viewerHex>"]` holds an array of `{id: uuid, community: origin, profile?: {name,picture,about?}}`. It **never stores invite codes, receipts or keys**. One entry is kept per origin. `latest()` is used to resume (`src/features/communities/join-journal.ts:10-88`).

Other per-viewer key: `buzz:klipy-customer-id:v1` holds a random UUID sent with GIF searches (`src/features/relay/gifs.ts:41,105-115`).

### 1.4 NIP-11 fetch
Native path: `nativeRelayInfo(community)` calls `relay_http(community, "/", "GET")` (`native.ts:315-322`).

Request (Rust `send`, unauthenticated branch, `relay.rs:708-769`):
- `GET https://host/` with header `Accept: application/nostr+json`.
- No auth and no redirects (`Policy::none()`).
- Connect timeout 10 s, total timeout 30 s (`relay.rs:640-652`).
- Response capped at 16 MiB (`MAX_RESPONSE`, `relay.rs:41`).

JS adds its own 30 s abort (`native.ts:104-108`).

The server returns NIP-11 on `GET /` only when `Accept` contains `application/nostr+json`. Otherwise it serves the WebSocket upgrade or the SPA (`buzz:crates/buzz-relay/src/router.rs:585-606`). The same document is also at `GET /info`.

The response must be a JSON object. Fields the client reads:

| field | use | source |
|---|---|---|
| `self` | relay signing pubkey (authority for relay-signed kinds) | `native.ts:330-332` |
| `name` | membership label | `native-api.ts:82`, `CommunityDialog.tsx:232` |
| `icon` | rail icon, sanitized | `community-icon.ts` |
| `supported_nips` includes `29` | channel creation (kind 9007) allowed | `native.ts:334-335`; Rust re-checks via a separate fetch, `relay.rs:610-638` |
| `read_state_snapshot` `{version:1, community_id:<uuid>}` | read-state snapshot capability | `src/features/relay/read-state-snapshot.ts:9-20` |
| `supported_extensions` includes `"buzz-gif"` + `gif:{provider:"klipy", search:"/gifs/search"}` | GIF search | `src/features/relay/gif-capability.ts:13-22` |

Server-side the struct also has `description`, `pubkey`, `contact`, `software`, `version`, `limitation{auth_required,...}`, `push`, `admin_api`, `federated_identity`, and `pairing_relay_url` (`buzz:crates/buzz-relay/src/nip11.rs:25-82`). The client ignores these.

Caching:
- **None in native.** NIP-11 is re-fetched on every `connectNativeTransport`, every `info` route call and every icon fetch.
- The only cache is a per-community promise for the GIF search path, which is forgotten on failure (`gifs.ts:146-167`).

### 1.5 Community icon (`src/features/communities/community-icon.ts:5-48`)
The NIP-11 `icon` is accepted only in one of three forms:
1. A string of ≤ 2048 chars that starts with `https://`, has a host, has no userinfo, and has no whitespace.
2. `data:image/svg+xml,` of ≤ 98,304 chars that matches the fixed Buzz emoji-avatar SVG template exactly (`rx` = 112 or 256, `fill` = `#rrggbb`, 1-128 chars of text). The client rebuilds this SVG from the template rather than trusting the original.
3. `data:image/(png|jpeg|gif|webp);base64,...` of ≤ 98,304 chars.

Fetch timeout is 12 s. On failure the client keeps the saved icon or shows initials (`src/features/communities/CommunityRail.tsx:117-121`). The icon is set server-side by admins through command kind 9033 (`nip11.rs` doc comment).

### 1.6 Join flow (native)
All calls go through `nativeCommunityRequest(community, route, body)` with a 25 s timeout (`src/features/communities/native-api.ts:18-267`). The order is set by `CommunityDialog.tsx:132-245`:

1. **`info`**: in parallel, `GET /` (NIP-11) and `GET /api/join-policy` (unauthenticated; `Accept: application/nostr+json` is sent here too) (`native-api.ts:49-93`).
   - Join-policy response (`buzz:crates/buzz-relay/src/api/invites.rs:113-127`): `{"policy":{"terms_markdown":str|null,"privacy_markdown":str|null,"age_attestation_required":bool,"version":str}}`, or `{}` when no policy is set. A 404 is treated as no policy.
   - Validation: `version` must be a string and `age_attestation_required` must be a bool.
   - Policy pages are also served as HTML at `GET /api/join-policy/terms` and `/api/join-policy/privacy` (`buzz:crates/buzz-relay/src/router.rs:374-381`).
2. **Profile probe** (`inspectProfile`, `src/features/communities/api.ts:52-88`): connect the native transport (NIP-11 + signer), then `POST /query` with `[{"kinds":[0],"authors":[viewer],"limit":5,"consistency":"strong"}]` and a 12 s timeout.
   - If the probe succeeds, the viewer is already a member. If the profile exists, offer "Open community", which saves the membership without publishing.
   - A 401/403 (`ReadError` kind `"denied"`) means "not a member", and the flow continues to admission.
3. **Policy acceptance** (only if a policy exists): `POST /api/invites/accept-policy`, NIP-98 signed by the Rust host.
   - Body: `{"code":"<code>","policy_version":"<version>","age_confirmed":true|false}`.
   - Success: `{"receipt":"<opaque>"}`. The client requires a non-empty string.
   - Errors: 404 `join_policy_not_configured`; 400 `join_policy_not_accepted` (version mismatch, or age required but not confirmed).
   - Server: `buzz:crates/buzz-relay/src/api/invites.rs:199-229`. The server handler does not verify NIP-98 on this route.
4. **Claim**: `POST /api/invites/claim`, NIP-98 required (the joining key; exempt from the membership gate).
   - Body: `{"code":"<code>","policy_receipt":"<receipt>"|undefined}`.
   - Client code check: `/^[a-zA-Z0-9._-]{1,256}$/` (`native-api.ts:107-112`).
   - Success 200: `{"status":"joined"|"already_member","community_id":"<uuid>","host":"<host>","role":"member"}`. The client accepts only those two statuses.
   - Errors: 403 `{"error":"invite_invalid"|"invite_expired"|"invite_exhausted"|"join_policy_required"}`, 429 `"too many invite claim attempts, slow down"` (10 per pubkey per 60 s).
   - Codes prefixed `v2.` are database-backed. Other codes go through the v1 HMAC path.
   - On `joined`, the server publishes NIP-43 kind 8000 (member added) and an updated kind 13534.
   - Server: `buzz:crates/buzz-relay/src/api/invites.rs:427-563`.
5. **Profile**: re-read the profile. If it is missing or differs, publish kind 0 (route `profile`, `native-api.ts:185-226`).
   - Content: `JSON.stringify({...existing, name, display_name: name, picture, about})`.
   - Limits: name 1-100 chars after trim, about ≤ 500, content ≤ 16,000 bytes.
   - Signed via `relay_sign`, sent via `relay_http POST /events`. The client requires `result.event_id === event.id && result.accepted === true`.
   - Then re-read and compare (`CommunityDialog.tsx:203-227`).
6. Call `communities.joined(...)`, then `journal.finish(entry)`.

Journal writes happen before steps 3/4 and before step 5. A storage failure blocks the remote operation (`docs/communities.md:150-160`).

An invite claim that went through is **not** undone if profile setup fails later.

### 1.7 Invite links
**Deep link format** (`src/features/communities/invite-link.ts:9-46`):
```
buzz://join?relay=<wss-or-https-origin, URL-encoded>&code=<code>[&policy_receipt=<r>]
```
Parser rules:
- Input ≤ 4096 chars and must match `/^buzz:\/\/join\/?\?/`.
- Host must be `join`; path must be empty or `/`; no userinfo or hash.
- Exactly one `relay` and one `code` (non-empty, ≤ 256 chars); at most one `policy_receipt` (non-empty if present).
- `relay` must pass `relayOrigin`.
- Result: `{community: origin, code}`. **`policy_receipt` is validated but discarded.**

A link is an *intent* only (`invite-intent.ts`). It pre-fills the join dialog for the current viewer; the user must still confirm.

OS ingress (`src-tauri/src/deep_links.rs:1-80`):
- Scheme `buzz` (`src-tauri/tauri.conf.json` `deep-link.desktop.schemes`).
- Rust queues up to 32 raw URLs. JS drains them with `deep_link_take` and listens with `deep_link_watch(channel)`.
- All parsing happens in TS.

**Minting** (route `invite`, owner/admin only): `POST /api/invites`, NIP-98 with `payload` required.
- Body: `{"ttl_secs":60..2592000,"max_uses":1..10000|null}`. The server defaults ttl to 72 h; `null` means unlimited uses.
- 200 response: `{"code","expires_at":unixSecs,"max_uses","uses_remaining","url":"https://<host>/invite/<code>"}`.
- 403: `"only relay owners and admins can create invites"`. 400: `"ttl_secs must be between 60 and 2592000"` / `"max_uses must be between 1 and 10000"`.
- Sources: `native-api.ts:157-184`, `admin-protocol.ts:9-31`, `buzz:crates/buzz-relay/src/api/invites.rs:51-90,278-423`.

The UI shares the server's `url`, an HTTPS landing page served by the relay SPA (`buzz:crates/buzz-relay/src/router.rs:143,509`). I did **not** find, in either repo, where that landing page builds `buzz://join?...`. A port should handle both forms: `https://host/invite/<code>` → community=`https://host`, code=`<code>`. This mapping is inferred, not confirmed.

### 1.8 Leave (NIP-43)
Route `leave` (`native-api.ts:252-265`, `src/features/communities/leave-protocol.ts`):
- Sign `{"kind":28936,"content":"","created_at":now,"tags":[["-"]]}`. Rust enforces this exact shape (`relay.rs:299-304`).
- Send with `POST /events`. Success means `event_id` matches and `accepted === true`.

Refusals the client recognizes (HTTP 400 `{"error": ...}`):
- `"invalid: you are not a relay member"` and `"invalid: relay membership is not enabled"` → treated as **already-absent**. Remove the community and purge device state.
- `"blocked: you are banned from this community"` → **access-revoked**. Remove the community but keep device state.
- `"invalid: relay owner cannot leave"` and `"community writes are temporarily unavailable"` → error; keep the membership.

Any other error keeps the membership so the user can retry.

Local cleanup: `communities.leave(id, {purge})` removes the membership, falls back to Personal space if it was selected, disposes the session, then purges the `origin:viewer` stores: view state, channel setups, quick reactions, thread follows, IndexedDB partition, outbox and read state (`service.ts:333-376`, `src/features/communities/device-state.ts`).

### 1.9 Roles and admin protocol
- **Roster** (`src/features/communities/roster.ts`): read `[{"kinds":[13534],"authors":[relaySelf],"limit":1}]` fresh.
  - The event must be kind 13534 with `pubkey == NIP-11 self`.
  - Tags have the form `["member", <hex>, "owner"|"admin"|"member"]`. An unknown role is treated as `member`; duplicate pubkeys keep the first entry.
  - No event means no roles.
  - `managesCommunity` = owner or admin.
- **Member commands** (`admin-protocol.ts:35-66`, Rust `valid_member_command` `relay.rs:325-336`), all with empty content:
  - kind 9030 add: `[["p",hex],["role","admin"|"member"]]`
  - kind 9031 remove: `[["p",hex]]`
  - kind 9032 role: `[["p",hex],["role","admin"|"member"]]`
  - Signed via `relay_sign`, sent with `POST /events`.
  - Refusal strings shown verbatim are listed at `admin-protocol.ts:70-94`, e.g. `"invalid: actor not authorized: must be admin or owner"`, `/^invalid: member not found: [0-9a-f]{64}$/`.
- The server also publishes kinds 8000/8001 (member added/removed) (`buzz:crates/buzz-core/src/kind.rs:398-402`).

### 1.10 Profile default rule
When a profile is saved in a community, it also becomes the local default profile. Exception: a `picture` whose origin is the current community **and** whose path starts with `/media/` is not copied into the default; the previous default picture is kept. Source: `src/features/communities/profile-default.ts`.

### 1.11 Out of scope
- `enterprise_login_gate`: a build-time allowlist (`BUZZ_BUILD_ENTERPRISE_AUTH_RELAYS`) with its own NIP-11 fetch (`src-tauri/src/enterprise_login_gate.rs:35-60`). Optional.
- Community creation is not implemented in the client (`docs/communities.md:142`).

---

## 2. Identity

### 2.1 Key storage
- Exactly **one human key**, stored in the OS credential store (`crates/credential-store/src/lib.rs:15-24`):
  - Service: `dev.local.buzz.foundation.identity` (release) or `dev.local.buzz.foundation.identity.debug` (`cfg!(debug_assertions)`).
  - Account: `human`.
- **The stored value is the bech32 `nsec1...` string**, UTF-8 (`src-tauri/src/identity.rs:310-323`).
- Backends:
  - macOS: Keychain generic password via `security-framework` (`identity.rs:170-212`, `lib.rs:186-202`).
  - Windows: Credential Manager via `keyring` 3.6.3.
  - Linux: Secret Service via `keyring`. Needs a desktop Secret Service with a default collection.
- Create-only semantics: `add` re-reads under a lock and refuses with `Occupied` if a value exists.
  - Windows/Linux lock file: `<OS-account home>/.buzz-foundation/credential-locks/<sha256(service\0account) hex>.lock`, taken with a non-blocking `try_lock`; contention returns `Busy` (`lib.rs:78-176`).
- **There is no file or environment fallback, and no legacy migration.** Missing, denied or corrupt storage fails closed and never triggers key generation (`docs/identity.md:9-35`).
- Errors map to user strings (`identity.rs:159-170,215-226`). Examples: Denied → "Secure storage access was denied...", Busy → "Another Buzz app is accessing secure storage. Retry shortly.", Corrupt → `MALFORMED`.

### 2.2 Key format, generation, import
- Import (`Key::parse`, `identity.rs:108-125`): trim; length must be 63; must start with `nsec1`/`NSEC1`; checked bech32; exactly 32 bytes; must be a valid secp256k1 scalar. Error: `"Enter a valid nsec private key"`. Hex import is **not** supported.
- Generate (`identity.rs:126-135`): 32 bytes from `getrandom`, retried until valid.
- Pubkey: x-only, lowercase hex (`identity.rs:136-144`).
- State machine `Unread | Missing | Ready(Key) | Closed` (`identity.rs:278-337`). `save` refuses if a key is already present, and the key is committed in memory only after the store write succeeds.

### 2.3 Tauri commands (identity)
JS service: `src/features/identity/service.ts:25-83`.

| command | args | returns |
|---|---|---|
| `identity_restore` | — | `string \| null` (hex pubkey) |
| `identity_create` | — | hex pubkey |
| `identity_import` | `{nsec}` | hex pubkey |
| `identity_export` | — | nsec string |
| `sign_out_wipe_refusal` | — | `string \| null` |
| `sign_out` | `{wipe, removeAgents}` | void; the app restarts |

Command sources: `identity.rs:1034-1057`, `src-tauri/src/sign_out.rs:753-760`.

### 2.4 Sign-out
`sign_out` (`sign_out.rs:1-80`, `docs/identity.md:68-190`):
- Writes a marker `.{identifier}--{service}.sign-out-pending` outside app data, closes the signer (`IdentityHost::close` → `State::Closed`), stops agents and restarts.
- The **next launch** deletes the key from the credential store and confirms a fresh read finds nothing (`identity.rs:265-276`), before any window opens.
- `wipe` also clears app data and WebView storage. Wipe is refused in debug builds.
- Any failure keeps the marker, and the next launch retries.

### 2.5 Signing path
JS `nativeRelaySigner(community)` (`native.ts:281-313`):
- `getPublicKey()` → `identity_restore`.
- `signEvent({kind,created_at,tags,content})` → `relay_sign {community: origin, event}`. Kind 30078 goes to `relay_kit_sign` instead.

Rust `Key::sign_bounded` (`identity.rs:51-80`):
- id = `sha256(JSON [0,pubkey,created_at,kind,tags,content])`.
- BIP-340 Schnorr with 32 bytes of aux randomness.
- Default size cap: 64 KiB of serialized JSON. Catalog kinds use `catalog::MAX_EVENT_BYTES`.

`relay_sign` validation (`relay.rs:225-322`) allows only:
- Kinds `0, 7, 9, 1984, 9000, 9001, 20001, 30030, 30177, 30315, 40003, 42000, 45010`.
- Kind 22242, exact shape (§2.6).
- Kind 5, only the message-deletion, managed-agent or workflow shapes. Coordinate (`a`) deletes must be by the author.
- Kinds 30620/46020 (workflow).
- Kind 9007, only if NIP-11 `supported_nips` has 29 **and** `self` is valid.
- Kinds 30175/30178 (catalog).
- Kind 40100 (canvas).
- Kind 28936 (exact `[["-"]]`).
- Kinds 9030-9032.

Everything else is rejected, including 24242 and 27235, so JS can never mint media or HTTP auth tokens.

Purpose-bound signers:
- `relay_kit_sign` (30078 recipes).
- `relay_channel_sign` / `relay_channel_publish` with route ∈ `channel-details | channel-lifecycle | identity-archive | member-administration`.
- `relay_sign_read_state` / `relay_sign_sidebar` (30078, NIP-44 self-encrypted).
- `relay_sign_reminder` (30300).
- `relay_direct_message` (41010).
- `relay_git_authorization`.

### 2.6 NIP-42 AUTH (WebSocket)
Live client (`src/features/relay/live.ts:720-758`):
1. On `["AUTH", challenge]`, sign:
```json
{"kind":22242,"content":"","created_at":<now>,"tags":[["relay","wss://relay.example.com"],["challenge","<challenge>"]]}
```
2. Send `["AUTH", <event>]`.
3. Wait for `["OK", <authId>, true, ...]`. `OK` with `false` is terminal ("Relay rejected live authentication"). If auth has not finished 10 s after the socket is created, the client reconnects (`live.ts:698-701`).
4. The client also checks that `auth.pubkey === viewer`.

Relay tag value is `wss://host[:port]` with no trailing slash.

Rust check (`relay.rs:262-283`):
- Content must be empty; exactly 2 tags.
- Exactly one `relay` tag whose `Url::parse` equals the community origin with scheme `wss` (trailing-slash-insensitive).
- Exactly one non-empty `challenge` of ≤ 4096 chars.

Server expects `relay` = `wss://<tenant host>` (`buzz:crates/buzz-relay/src/api/bridge.rs:348-365`).

### 2.7 NIP-98 HTTP auth (all authenticated `relay_http` calls)
`send()` (`relay.rs:708-769`) signs a **fresh event per request attempt**. Auth events are never cached or reused.
```json
{"kind":27235,"content":"","created_at":<now>,
 "tags":[["u","https://relay.example.com/query"],["method","POST"],
         ["payload","<sha256 hex of exact body bytes>"],["nonce","<uuid v4>"]]}
```
- `u` = `Url::join(origin, path).to_string()`, the full URL including any query (e.g. workflow runs `?limit=20&before=...`).
- `payload` is present whenever a body is sent: every POST.
- `nonce` is always present. The server has a replay guard on event IDs.

Request headers:
- `Authorization: Nostr <base64-standard(JSON(event))>`.
- `Content-Type: application/json` on all authenticated requests.

Rules:
- **Never retry** a request after a transport error; the error is returned as "Relay request could not be confirmed".
- Only the response headers `content-type`, `retry-after` and `server-timing` are forwarded to JS (`relay.rs:771-794`).

Which requests are authenticated: every POST, plus `GET /workflows/<id>/runs`. `GET /` and `GET /api/join-policy` are not.

Server verification (`buzz:crates/buzz-auth/src/nip98.rs:55-232`):
- `created_at` within **±60 s**.
- Exactly one `u`, compared after normalizing (lowercase scheme/host, trailing `/` stripped).
- Exactly one `method`, case-insensitive.
- At most one `payload`, which must be 64 lowercase hex and match the body if present.
- Expected URL = `https://<tenant host><path>` (`buzz:crates/buzz-relay/src/api/bridge.rs:335-346`).
- `/events`, `/query` and `/count` do not *require* `payload`. `/api/invites` does.

Git variant `relay_git_authorization` (`relay.rs:196-224`): tags `[["u",repoUrl],["method","GET"]]` with no nonce or payload. Returns only the base64 token. The relay accepts it for 60 s across one repository's routes.

### 2.8 Blossom kind 24242 auth
`blossom_auth(url, verb, content, extra_tags)` (`relay.rs:820-856`) builds:
```json
{"kind":24242,"created_at":<now>,"content":"Upload attachment",
 "tags":[["x","<sha256 hex>"],["t","upload"],["server","relay.example.com:8443"],["expiration","<now+60>"]]}
```
- For `get`: content `"Get buzz-media"`, no `x` tag, `t=get`.
- `server` is host plus explicit port only (`url[BeforeHost..AfterPort]`).
- Header: `Authorization: Nostr <base64-standard(JSON)>`.
- A fresh token is signed per request.

Server checks (`buzz:crates/buzz-media/src/auth.rs:65-312`):
- Valid signature, kind 24242, **non-empty content**.
- Strict mode (NIP-FI active): exactly one each of `t`, `expiration`, `server`; `created_at ≤ now+5`; `now − created_at ≤ 60`; `expiration ≤ created_at+60`. Permissive mode allows a 1 h age and an optional `server`.
- `expiration` must be in the future.
- Upload requires an `x` tag equal to the `X-SHA-256` header.
- `get` requires `x` or `server`.

### 2.9 NIP-44 in Rust (self-encryption only, never generic)
- `kit_cipher`: NIP-44 v2 encrypt/decrypt to self, for channel recipes (`identity.rs:1059-1083`).
- `decode_read_state` / `sign_read_state`: kind 30078 with tags `[["d","read-state:<32 hex>"],["t","read-state"]]`. Requires `created_at` within ±60 s of now; plaintext ≤ 128 KiB; ≤ 16 events per decode (`identity.rs:766-880`).
- `decode_sidebar` / `sign_sidebar`: kind 30078 with `d`=`t` ∈ `channel-sections|channel-stars|channel-mutes|channel-sort` (`identity.rs:566-571,635-750`).
- Reminders: kind 30300, NIP-ER (`src-tauri/src/relay/reminders.rs`).

Every decrypt first verifies the signature and checks `pubkey == self` and the kind.

---

## 3. Connection lifecycle

Scope: how the client opens, authenticates, subscribes to, recovers and publishes on a
community relay.

### 3.0 Two host topologies (read this first)

The TS app has two transports. Only one of them is the shipped native app.

| | Packaged native (Tauri) — **port this** | Dev broker (`just web` / `just desktop`) |
|---|---|---|
| Selected by | `nativeIdentityEnabled()` → `connectNativeTransport` | else `connectBrokerTransport("", signal, id)` |
| Who opens the WS | **The webview JS**: `new WebSocket("wss://<host>")` in `live.ts` | Node broker process runs the same `live.ts` and re-streams to the browser over SSE (`POST /api/relay/<c>/stream`) |
| AUTH signer | Tauri `relay_sign` (kind 22242 validated in Rust) | broker `finalizeEvent` with host key |
| Finite reads | `POST <origin>/query`, NIP-98 signed in Rust (`relay_http`) | broker `/api/relay/<c>/query` → upstream `/query` |
| Ordinary writes | **HTTP `POST <origin>/events`**, NIP-98, via Rust `relay_http` | **WS `EVENT` on the live socket** (`stream.traffic.publish`), no HTTP fallback |
| Presence (kind 20001) write | WS `EVENT` on the live socket | WS `EVENT` via `/stream-presence` |

Sources: `src/features/communities/connection.ts:18-22`; `src/features/relay/native.ts:324-339`;
`src/features/relay/transport.ts:1117-1231` (signed transport: WS via `subscribeRelayTraffic`,
writer = `signedPost(.../events)`); `browser-host/relay-broker.mjs:1634-1712, 2575-2622`.
The Rust side has **no relay WebSocket** (`tokio-tungstenite` is used only by
`src-tauri/src/pairing/relay.rs`). `docs/websocket-publication-profile.md:10-15` states WS-first
publication is "development-host integration, not packaged-desktop support".

**Port recommendation:** one native WS per community (as `live.ts` does) plus a NIP-98 HTTP
client for `/query` and `/events`. The rest of this section describes `live.ts` behaviour
(shared by both topologies) and the native HTTP write path. The broker SSE layer is a
dev-only IPC wrapper. §3.9 summarizes it in case you want a reference.

### 3.1 Opening the socket

- **URL**: `relayOrigin(input)` normalizes any `wss://` or `https://` input to `https://host[:port]`
  (no path or trailing slash; a trailing DNS dot is stripped). The WS URL is
  `httpOrigin.replace(/^http/, "ws")`, so it is `wss://host`.
  `src/features/communities/destination.ts:2-23`; `src/features/relay/transport.ts:1123,1179`.
  Rust `origin()` accepts only `https://`, no credentials, path `/`, no query or fragment
  (`src-tauri/src/relay.rs:58-72`).
- **Pre-connect**: `connectNativeTransport` first does `GET /` with `Accept: application/nostr+json`
  (NIP-11), and requires `info.self` to be a 64-hex key (= `relayAuthor`). `supported_nips` containing 29
  enables kind 9007. `info.read_state_snapshot` sets the read-state community.
  `src/features/relay/native.ts:315-345`.
- **Connect deadline**: `provideRelay.retry()` aborts the whole transport connect (NIP-11 +
  identity) after **8 000 ms** and shows "Relay connection timed out. Try again."
  (`src/features/relay/service.ts:187-197`). The WS is opened later, when the session calls
  `transport.subscribe()` (`src/features/relay/session.ts:2319`).
- **Cardinality**: **one socket per community session** (per `(origin, viewer)`). Communities
  are acquired lazily and kept in a `Map`, so N retained communities give N sockets
  (`src/features/communities/service.ts:68,125-145`). All channel, global and presence
  routes are multiplexed on that one socket (`live.ts:204`: "One authenticated socket").
- **Headers**: none. The browser `WebSocket` API cannot set them. The default factory is
  `(url) => new WebSocket(url)` (`live.ts:211`). A Rust port must not depend on
  `Authorization` or `Origin`. Auth is NIP-42 only.
- **Frame limits**: an inbound frame that is not a string or is >1 MiB (`1024*1024`) is logged and
  dropped (`live.ts:707`). Outbound publication frames are capped at 64 KiB (catalog kinds
  use `CATALOG_EVENT_BYTES`) (`socket-requests.ts:39-44`). The server rejects client frames
  >512 KiB by default (`buzz:crates/buzz-relay/src/config.rs:14`, `BUZZ_MAX_FRAME_BYTES`).

### 3.2 NIP-42 AUTH handshake

Sequence (`src/features/relay/live.ts:696-760`):

1. On socket creation, start `deadline = 10 000 ms` → `reconnect("Live authentication timed out")`.
   The server is stricter: it closes unauthenticated sockets after **5 s**
   (`buzz:crates/buzz-relay/src/connection.rs:30,680-693`). In practice the server close comes first
   and triggers `onclose` → reconnect.
2. The server sends `["AUTH", "<challenge>"]` immediately after upgrade
   (`buzz:crates/buzz-relay/src/connection.rs:626-632`). **The client waits for it. It never sends
   REQ or EVENT before AUTH succeeds**: `pump()` returns early while `!authenticated`
   (`live.ts:523`), and publications stay queued in `socket-requests`.
3. On the first `AUTH` frame only (`authenticating` latch), sign:
   ```json
   {"kind":22242,"content":"","created_at":<now s>,
    "tags":[["relay","wss://host"],["challenge","<challenge>"]]}
   ```
   Then send `["AUTH", <signed>]`. Native `relay_sign` requires exactly these two tags, the
   relay tag equal to the `wss` origin, empty content, and a challenge of 1–4096 chars
   (`src-tauri/src/relay.rs:260-284`). The server normalizes trailing slashes and localhost,
   and allows ±60 s clock skew (`buzz:crates/buzz-auth/src/nip42.rs:17-35,74-81`).
4. If `auth.pubkey !== viewer`, the result is terminal "Live signer does not match viewer". A signing exception is
   terminal "Live authentication signing failed".
5. `["OK", authId, true, ...]` sets `authenticated = true`, `connection = "connected"`, starts the 60 s
   recovery timer (§3.5), then calls `pump()` (publications first, then REQs) and `notify()`.
   `["OK", authId, false, reason]` is terminal: "Relay rejected live authentication". There is
   **no automatic retry**. The connection status becomes `error` and the user must press Retry.
   Server reasons include `auth-required: verification failed`, `auth-required: already authenticated`,
   and `auth-required: agent owner recorded; reconnect` (`buzz:crates/buzz-relay/src/handlers/auth.rs:318,328,472,2095`).
6. **Re-auth after reconnect**: every new socket repeats steps 1–5 with a fresh challenge.
   There is no session resumption.

`auth-required:` at runtime: the client never sends REQ before AUTH, so the server's
`CLOSED <sub> "auth-required: not authenticated"` (`buzz:crates/buzz-relay/src/handlers/req.rs:84-91`) should not
occur. If it did, it is an ordinary non-transient CLOSED: the route goes to `error` (§3.4).
On publication `OK false "auth-required: ..."` is a *proven rejection* (§3.7). The client has
no "re-AUTH and resend" path.

### 3.3 Subscription management

**Route model** (`live.ts:183-199, 369-454`). A *route* is a logical interest. A *wire* is the
REQ subscription id currently carrying it.

- Route ids: `"profiles"`, `"membership"`, `"observer"` (only when agent observation or archive
  capture is on), `"presence"` (only when presence authors are non-empty), `"channel:<id>"`
  (singleton), `"batch:<n>"` (≤10 joined channels).
- **Wire (REQ sub id) format: `live-<n>`**. `n` is a per-subscription counter shared with batch ids
  (`live.ts:555`). Every (re)dispatch gets a fresh id. Old wire ids are fenced in the `wires`
  map *before* `["CLOSE", wire]` is sent (`live.ts:288-297`), so late frames and the server's CLOSED ack
  are ignored.
- Each REQ carries one route. Batching means up to 10 channels per REQ, not several routes per REQ.

**Interest input** (`session.ts:2184-2211`). Interests are joined, non-cached roster channels
plus demanded (open or previewed) accessible channels. `joined` is the roster subset that may be batched.
`prioritize(demandedChannels())` (≤64 ids, `live.ts:1031-1035`) puts those channels in singleton
routes and dispatches them first. `update(ids, joined)` validates ≤1024 ids matching
`^[a-zA-Z0-9_-]{1,128}$` (`live.ts:158-170`).

**Partitioning in `sync()`** (`live.ts:369-454`):
- Capacity is `LIVE_CHANNEL_CAPACITY = 1022`, minus 1 if observer is on, minus 1 if presence is on.
  Overflow channels get a route with `status:"limited"`, no wire, and the error
  "Live channel capacity reached; finite reads remain available".
- A channel that is not joined, or that is prioritized, becomes `channel:<id>`. Other joined channels are
  chunked into `batch:<n>` of `LIVE_BATCH_SIZE = 10` (the relay's `max_filters`=10:
  `buzz:crates/buzz-relay/src/protocol.rs:12`).
- Healthy wires are never rebalanced. If a batch member is removed, a **replacement** is created for the
  survivors (`replace()`, `live.ts:323-344`). The replacement uses a fresh wire, the original `since`,
  `limit:0`, and a single filter with all survivor ids. The old wire stays open (`previous`) until the
  replacement's EOSE. At most 2 wires exist per route.
- New routes get `since = floor(now/1000) - 300` (5 min look-back) (`live.ts:430`).

**Dispatch (`pump()`, `live.ts:521-637`)**:
1. Drain queued publications first (≤3 sent but unacknowledged), each gated by `admission.delay()`.
2. Then pending routes, sorted by rank: globals (rank −2) first, then by priority index. At most
   `SETUP_CONCURRENCY = 4` routes may wait for EOSE at once (`live.ts:22`).
3. Each REQ arms a per-route **10 000 ms** setup deadline. If it expires: `fail(route, "Live subscription setup timed out; retry available", transient=true)`.

**Every live REQ** (V = viewer hex, S = route.since, L = 500 = `LIVE_REPLAY_LIMIT`, or 0 for live-only routes):

| Route | When opened / closed | Filters (one REQ) |
|---|---|---|
| `profiles` | Always while the socket is authenticated. Never closed except on dispose. | `{"kinds":[0,10100,30177],"since":S,"limit":500}`, `{"kinds":[30315],"#d":["general"],"since":S,"limit":500}`, `{"kinds":[30030],"#d":["buzz:custom-emoji"],"since":S,"limit":500}` |
| `membership` | Always | `{"kinds":[44100,44101],"#p":[V],"since":S,"limit":500}`, `{"kinds":[30078],"authors":[V],"#t":["read-state"],"since":S,"limit":500}`, `{"kinds":[30300],"authors":[V],"since":S,"limit":500}` |
| `channel:<id>` (first open) | Channel in interests and (not joined, or prioritized). Closed when it leaves interests or changes class. | `{"kinds":CK,"#h":["<id>"],"since":S,"limit":500}` |
| `batch:<n>` (first open) | Joined channels, chunks of ≤10 | one filter **per channel**: `{"kinds":CK,"#h":["<id_i>"],"since":S,"limit":500}` ×≤10 |
| batch replacement / 60 s renewal | Member removed, or recovery tick | `{"kinds":CK,"#h":[id_1..id_k],"since":S_orig,"limit":0}` |
| `presence` | `watchPresence(authors)` non-empty (≤256, sorted unique). Replaced on author-set change; removed when empty. | `{"kinds":[20001],"authors":[...],"since":S,"limit":0}` (always live-only) |
| `observer` | Agent activity observation or archive capture on | `{"kinds":[24200 and/or 44200],"#p":[V],"since":<now s at dispatch>}`. **No `limit`**. On renewal, `since` may be `metricsSince` (≥ now−60 s, ≥ now−240 s at dispatch) for kind 44200. |

`CK = CHANNEL_KINDS = [9,40002,40008,45001,45003,40099,40100,40003,5,9005,7,39000,39002,39005,20002]`
(`live.ts:200-203`). Filter construction is at `live.ts:572-635`.

**Live views mapped to routes** (there are no other REQs):
- Channel timeline live tail, threads, reactions, edits, deletes, typing (20002), channel metadata
  (39000/39002), and DMs (DMs are ordinary channels identified by a channel UUID,
  `transport.ts:555-577`): these use the channel and batch routes. A thread view adds no REQ. It refreshes
  over HTTP when its channel establishes (`session.ts:2535-2541`).
- Sidebar / global: `profiles` (profiles, agent catalog 30177, user status 30315, custom emoji 30030)
  and `membership` (membership hints 44100/44101, read-state 30078, reminders 30300).
- Unread counts and the inbox have no REQ. They are derived from channel traffic plus HTTP reads.
- Presence: the `presence` route. Agent activity: `observer`.

**Closing**: `["CLOSE", wire]` is sent when a route is removed or replaced, or when a previous wire is superseded
by EOSE (`live.ts:288-301, 880-883`). On socket loss, all wires are dropped without CLOSE.
Server limits: 1024 subscriptions per connection (`CLOSED "error: too many subscriptions"`),
128 explicit `#h` values per REQ (`"restricted: too many explicit channels"`),
`buzz:crates/buzz-relay/src/handlers/req.rs:26,43,73-79,97-105`.

### 3.4 EOSE, CLOSED, NOTICE, EVENT routing

Frame dispatch is in `live.ts:705-904`. The frame's sub id is looked up in `wires`. Unknown ids are ignored.

- **EVENT** (`live.ts:804-879`):
  - The event goes through `eventDto()`: full schnorr verification and strict field validation
    (`events.ts:85-135`). **One invalid event fails the whole route**:
    `fail(route, "Relay supplied invalid live traffic")`, which is non-transient.
  - Destination channel: the single `h` tag. If the event has no `h` tag and is kind 39000/39002,
    its `d` tag is used. An event with `h` tags that are all outside the wire scope is dropped.
    Kind 20002 without a resolved channel is dropped.
  - A route still on its previous wire drops events outside the narrowed scope.
  - Delivery: `presence` route → `callbacks.presence` (only kind 20001 from watched authors).
    `observer` → `capture` / `telemetry`. Everything else → `callbacks.receive([event], provenance)`
    with `phase: "live"` if the route is live-only or past EOSE, otherwise `"replay"`.
    Provenance carries `channelId` or `sourceChannels`.
  - Before EOSE, `route.count++` counts replay events.
- **EOSE** (pending routes only): clear the setup deadline, close the `previous` wire, set `status = "live"`,
  and set `replay = count >= 500 ? "limited" : "unknown"`. Then call `callbacks.established(channelIds | channelId)`
  for channel and global routes (not presence or observer). EOSE marks the switch from stored to live
  events. **It does not prove history is complete** (`docs/relay-queries.md:756-760`).
- **CLOSED** `[ "CLOSED", wire, reason ]`. The reason is truncated to 512 chars.
  `fail(route, reason, transient = reason starts with "rate-limited:" or "error:")` (`live.ts:893-903`).
  `fail()` (`live.ts:455-520`):
  - If the route has an established previous wire with the same scope and the failure is
    transient, keep the old wire. The minute timer retries the renewal (`retryRenewal = true`,
    except for quota failures).
  - `restricted: not a channel member` → `callbacks.denied(id, reason)` for each channel in scope.
    The session revokes access (`session.ts:2559-2561`). Then `sync()` runs.
  - `restricted: channel access revoked` (server: `buzz:crates/buzz-relay/src/handlers/side_effects.rs:264`): the route
    goes to `error`. The session treats a *new* occurrence as a hint and schedules a coalesced roster
    refresh (`session.ts:2476-2503`), plus suspend and recheck for previews. The CLOSED frame
    itself does not prove the user lost access.
  - `rate-limited:*` → §3.6.
  - Other reasons → `status:"error"`. There is no automatic retry, except the 60 s renewal path for
    retained batches. The user's Retry resets it.
  - A CLOSED on a `previous` wire only closes that wire (`live.ts:797-802`).
- **NOTICE**: **not handled**. It is only logged by `logSocketFrame` (`live.ts:718`). The server
  sends NOTICE together with CLOSED for scope and auth errors (`buzz:crates/buzz-relay/src/handlers/req.rs:63,85`). The CLOSED frame is
  what the client acts on.
- **OK**: correlated in this order: AUTH id, presence receipt id, then `socket-requests` pending
  publication ids (`live.ts:748-790`). Any other OK is ignored.

### 3.5 Reconnect, backoff, recovery timers

| Mechanism | Constant | Source |
|---|---|---|
| Socket reconnect backoff | Equal jitter: `delay * (0.5 + random/2)`, `delay` starting at 500 ms and doubling to a 30 s cap (250–500 ms first, 15–30 s capped). A full minute authenticated (`recover`) resets it; a brief authentication does not (buzz-app #996) | `live.ts` |
| Max automatic reconnects | None: transient disconnects and auth timeouts retry while the session is active. Auth rejection or signing failure stays terminal until Retry | `live.ts` |
| Attempt counter reset | **Only in `retry()`** (user action). It is *not* reset when AUTH succeeds. | `live.ts:1055-1069` |
| Auth deadline | 10 000 ms (server: 5 s) | `live.ts:698-701` |
| Per-REQ setup deadline | 10 000 ms | `live.ts:558-565` |
| Recovery / renewal tick | `LIVE_RECOVERY_INTERVAL = 60_000` ms. Starts on AUTH OK. Re-armed each tick while the same generation is authenticated. | `live.ts:20, 345-368, 756` |
| Server ping | WS Ping every 30 s. 3 missed pongs → server closes | `buzz:crates/buzz-relay/src/connection.rs:1070-1102` |
| Client heartbeat / idle timeout | **None** on the direct socket. It relies on `onclose`/`onerror` and server pings. Browsers auto-pong. A Rust client must answer Ping. | (absence in `live.ts`) |

Triggers: `ws.onerror` → reconnect("Live connection interrupted"). `ws.onclose` →
reconnect("Live connection closed"). A socket-factory throw → reconnect (`live.ts:687-695, 905-906`).
**There are no online/offline, visibility or sleep/wake triggers on the live socket.**
`service.ts:341-363` listens to `window.online` and `visibilitychange`, but only
(a) calls `service.retry()` while the snapshot is still a *cached* local-first session that never
connected, or (b) on becoming visible, calls `channels.refreshList()` (HTTP). The UI "Retry live updates" button
(`src/bundled/channels/LiveStatus.tsx:94`) calls `session.live.retry()` (`session.ts:943-949`).
That retries the roster, rechecks previews, re-demands channels and calls `traffic.retry()`.

`reconnect(reason)`: `clearSocket()` increments `generation`, clears `authenticated` and all timers,
fails in-flight publications ("Socket connection interrupted", `sent` = whether the event was dispatched),
and closes the socket. Then the status becomes `retrying`, all non-limited routes go to `pending`, and a timer is set.
`connect()` then **clears all routes and rebuilds them from interests**, so `since` becomes
now−300 s again and `limit` returns to 500 (initial replay) (`live.ts:638-686`; test
`live.test.ts:1318-1362`).

**Resubscription and gap fill** — `since` is not taken from the last seen event. Instead:
- On the first transition away from `connected` (`session.ts:2445-2475`): increment `liveGeneration`, clear
  typing, mark reminders, heads, unread and inbox stale, cancel finite reads (`requests.invalidate()`),
  drop catch-ups, and interrupt workflows.
- On a global `established()` (once per generation, `session.ts:2506-2533`): refresh the roster, then
  on the next tick reconnect profiles, agent library, catalog, channel activity, emoji, statuses,
  unread and inbox feed, recover reminders, and run registered refreshers. All of this uses HTTP.
- On a channel `established(ids)` (`session.ts:2534-2552`): refresh thread views of that channel,
  `staleHead(id)`, and queue an HTTP **head catch-up** (`store.ts:2392-2431`) if the channel's
  window is retained. Otherwise the catch-up is deferred until demand. Catch-up concurrency is 1, or 2 for the
  selected channel (`session.ts:2259-2289`). The head filter is the window filter
  `{"kinds":[9,40002,40008,40099],"#h":[id],"limit":20,"top_level":true,"include_aux":true,"include_summaries":true}`
  (`window.ts:14-28`), validated against a relay-signed kind 39006 bound.
  A ReadError with `retryAfterMs` sets `retryAt` for the whole queue.
- 60 s tick (`recover()`): for every live **batch** route with no pending replacement, call `replace()`.
  This re-sends the REQ with `limit:0` on a fresh wire. The tick also re-pends retained renewals and calls
  `callbacks.recover()`, which runs `refreshRoster()` and `unread.reconnect()` (`session.ts:2554-2558`).
  Singleton routes are not renewed. The socket is not reset.

`retry()` (`live.ts:1055-1069`): resets `attempts` to 0. If authenticated, sets each `error` route to
`pending` with counts and quota retries reset, then pumps. If not authenticated, calls `connect()`.

### 3.6 Quota and rate-limit handling

Server strings (all `rate-limited:` prefixed):
- `rate-limited: quota exceeded; retry in <N>s`. WS budget: `human_ws_events_per_sec` (default 10)
  over a 5 s window, which is 50 REQ/COUNT/EVENT frames per 5 s per pubkey. Persistent EVENTs are also checked
  against `human_messages_per_min` (default 60). HTTP: `human_api_calls_per_min` (default 300) → HTTP 429.
  `buzz:crates/buzz-auth/src/rate_limit.rs:89-145`; `buzz:crates/buzz-relay/src/admission.rs:9,40-45`;
  `buzz:crates/buzz-relay/src/rejection.rs:66-138`; `buzz:crates/buzz-relay/src/api/bridge.rs:30-60`.
- `rate-limited: shared admission unavailable` (Redis down; HTTP status 503).
- `rate-limited: too many concurrent requests` (handler semaphore full, `buzz:crates/buzz-relay/src/connection.rs:1231,1264,1286`).
- A rejected REQ gets `CLOSED`. A rejected EVENT gets `OK false`. NIP-01-correct correlation is guaranteed
  (`buzz:crates/buzz-relay/src/rejection.rs:1-45`).

Client-side WS admission is shared per `(origin, viewer)` and survives socket replacement
(`createLiveAdmission`, `live.ts:26-59`; owner map `host-admission.ts:6-38`, cap 64 principals):
- `pause(N)` sets `cooldown = max(cooldown, now + (N+1)*1000)`. The extra second covers Redis
  whole-second rounding. `delay()` gates **both** publications and REQ setup in `pump()`.
- **REQ CLOSED rate-limited** (`live.ts:475-503`): N comes from the regex
  `^rate-limited: quota exceeded; retry in (\d+)s$`, else 5.
  - N ≤ 60: `pause(N)`. If `++quotaRetries ≤ 3` (`MAX_QUOTA_RETRIES`) the route goes back to `pending`.
    Otherwise every unsent pending route is set to `error` "Live request cooldown retries exhausted; retry available".
  - N > 60 or unsafe: `pause(min(N, 86400))`, and every unsent pending route is set to `error`
    "Unsupported live cooldown; automatic setup stopped".
- **Publication OK false rate-limited** (`live.ts:778-788`): `pause(hint ? min(N,86400) : 5)`.
  Here the regex is the looser `/retry in (\d+)s$/`.
- **Presence OK false rate-limited** (`live.ts:761-776`): `pause(hint ? N : 86400)`, using the exact quota regex.
  So a presence refusal *without* a hint (`shared admission unavailable` or `too many concurrent requests`)
  pauses all live admission for 86 401 s. This looks like a fail-closed choice, and it may be unintended. Flag it.
- Presence send gate: at most one in flight, plus a 5 s minimum gap (`presenceSent`) (`live.ts:37-53`).

HTTP admission (native `/query` and `/events`, `http-admission.ts:164-303`):
- Lane per `(origin, viewer)`: 6 active, queue ≤128, `prepare` ≤128. Foreground runs first. No pacing.
- `apiFailure`: HTTP 429 with `^rate-limited: quota exceeded` → `quota:"api"`,
  `retryAfterMs = (N+1)*1000`. N defaults to **60** if there is no hint and is capped at 86 400.
  The lane pauses, and every queued ticket is rejected with `ApiPaused` ("Relay requests paused; retry in Ns", status 429).
- The presence snapshot lane has a 5 s start gate and one flight at a time (`http-admission.ts:196-208`).

**UI** (`src/bundled/channels/LiveStatus.tsx:17-98`; policy in `docs/relay-queries.md:850-860`):
- A route counts as quietly recovering when it is connected and `pending`, and its error matches the exact quota
  regex with N ≤ 60. That state shows only in Channel Settings → Diagnostics as
  "recovering automatically after rate limiting; awaiting confirmation" and "Last rejection: …".
  The error text stays until a fresh EOSE.
- Anything else shows a warning toast, "Live updates need attention", with a
  "Retry live updates" button. Triggers: connection `unavailable`/`retrying`/`error`, a route `error`/`limited`, a head
  error, a roster error or deferral, or exhausted quota retries.

### 3.7 Publishing

**Native writes = HTTP**, not WS (§3.0). Flow: outbox → `writer.sign` (Tauri `relay_sign`) →
`writer.publish` → `signedPost(signer, "<origin>/events", event)` → Tauri `relay_http`
(`transport.ts:1211-1231`; `native.ts:833-858`).

- **Request**: `POST https://host/events`. The body is the signed event JSON (≤`MAX_BODY`). Headers:
  `Authorization: Nostr <base64(JSON(kind-27235 event))>` and `Content-Type: application/json`.
  The 27235 tags are `["u","https://host/events"]`, `["method","POST"]`,
  `["payload",<sha256 hex of body>]` and `["nonce",<uuid v4>]`, with `created_at = now`
  (`src-tauri/src/relay.rs:708-761`). reqwest: connect timeout 10 s, total 30 s, no redirects
  (`relay.rs:640-652`). JS adds a 30 s abort (`native.ts:105-108`). The Rust host **never retries
  after a transport error** (`relay.rs:763`). Rust also rejects writes of kinds 9002/9008/9021/9022/41012/9035/9036 and 41010
  through this path, and admits 30078 and 30300 only after validation (`relay.rs:669-694`).
- **Response (server)** (`buzz:crates/buzz-relay/src/api/bridge.rs:1158-1220`):
  - 200 `{"event_id":"<hex>","accepted":true|false,"message":"<string>"}`
  - 400 `{"error":"<reason>"}` for ingest rejection (`invalid:`, `restricted:`, `blocked:` …) or bad JSON
  - 409 `{"error":...}` for a canvas/workflow CAS conflict
  - 403 for auth failure, 429 for quota, 503 for shared admission unavailable, 500 `{"error":"internal server error"}`
  - Duplicates: a generic duplicate gives `200 accepted:true "duplicate:"`. A **duplicate reaction gives
    `200 accepted:false "duplicate: reaction already exists"`** (`buzz:crates/buzz-relay/src/handlers/ingest.rs:3286-3296,3457-3462`).
- **Client mapping** `acceptPublish` (`transport.ts:1313-1359`):

| HTTP result | Outcome |
|---|---|
| 409 | `PublishRejected("conflict: the relay state changed; reload before writing again")` → **failed** |
| 400, 401, 403, 404, 413, 422 | `PublishRejected("Relay rejected the message (<status>)")` → **failed** |
| other non-2xx with `quota:"api"` (429 quota) or body `sent:false` | `PublishRejected(error)` → **failed** |
| other non-2xx (500, 502, 503 not-quota …) | `Error("Relay delivery could not be confirmed (<status>)")` → **unknown** |
| 200, `event_id` ≠ id or `accepted` not boolean | `Error("Relay returned an invalid delivery receipt")` → **unknown** |
| 200 `accepted:false` | `PublishRejected(message)` → **failed** |
| 200 `accepted:true` | resolve(message) → **accepted** |
| `ApiPaused`/`ApiNotSent` before dispatch | `PublishRejected` → **failed** |
| Rust transport error ("Relay request could not be confirmed") or timeout | **unknown** |

- **Old-event guard** (`native.ts:836-855`): if `created_at < now − 15 min`, the client does not re-POST. It
  queries `{"kinds":[k],"ids":[id],"authors":[pk],"limit":1,"consistency":"strong"}`. If found → accepted.
  If not → `PublishRejected("This event is too old to retry…")`.

**WS EVENT → OK** (dev broker for all writes; native for presence only). `socket-requests.ts:26-142`:
- Bounds: pending ≤32 ("Publication capacity reached"), ≤3 sent and unacknowledged, a duplicate id in flight is rejected,
  frame ≤64 KiB, receipt text ≤16 KiB. The **10 000 ms deadline** includes time queued before AUTH
  or cooldown.
- `["OK", id, true, msg]` → resolve(msg). The message may be `"duplicate:"`, which counts as success.
- `["OK", id, false, msg]`: proven rejection (`sent:false`) only if `msg` matches
  `^(invalid|blocked|restricted|auth-required|rate-limited):`. `conflict:`/`forbidden:` also count for kinds
  30620 and 46020, and `conflict:` for kinds 40100 and 45010. **Anything else (`error:`, `duplicate:` with false,
  unknown prefixes) → uncertain (`sent:true`)**.
- A malformed OK (wrong arity or types) is uncertain. A socket close or abort after dispatch is uncertain. Cancel or timeout before
  dispatch is `sent:false`.
- **No replay on reconnect.** Pending jobs are failed by `clear()` (`socket-requests.ts:134-140`). The
  outbox owns any retry, and retry is user-initiated.
- Broker → browser mapping (`relay-broker.mjs:2575-2622`): success → 200
  `{"accepted":true,"event_id","message"}`. A proven `conflict:` → 409. Otherwise → 503
  `{"error": <rate-limited refusal or "Socket publication could not be confirmed">, sent:false?}`.
  A missing live owner → 503 `{"error":"Publication socket unavailable","sent":false}`.
  **There is no HTTP fallback** (`docs/relay-queries.md:684-694`; profile doc line 10:
  "no … automatic publication replay or HTTP fallback for uncertain writes").

**Outbox delivery** (`outbox.ts:81-90, 380-560`; bounds `docs/relay-queries.md:401-404`):
- States: `sending | accepted | unknown | failed | seen`. 3 concurrent deliveries. The default
  **10 000 ms** deadline (`timeoutMs`) starts at enqueue and includes sign and persist time. The app does not override it.
- When the deadline fires after the publish call started, the outcome is `unknown`. Before that, it is `failed`.
  `PublishRejected` → `failed`. Retry reuses the saved signed bytes. Restored pending items become `unknown`.
  Nothing is published automatically after a restart.
- After `accepted`, or after an uncertain publish, `confirm(event)` reads `[{ids:[id],limit:1}]` over HTTP
  at 0, then +500, +1500, +4000 and +10 000 ms, until the outbox sees the echo (`seen`) (`session.ts:579-602, 290-293`).

**Presence publish** (WS only, `live.ts:922-985`): kind 20001, content `"online"|"away"|"offline"`,
no tags. Returns null if not authenticated or busy. Returns `{retryAfterMs}` if gated. A 10 s receipt timeout → false.
Renewal cadence (`src/features/presence/presence.ts:14,226-285`): every 60–65 s (`60000 + rand*5000`).
Unsent (null) → retry after 5–6 s. Gated → retry after `retryAfterMs + 50` ms. The first send waits 250–1000 ms
of jitter unless it is immediate. An accepted `offline` stops renewal. A Web Lock `buzz-presence:<partition>`
makes sure only one window publishes.

### 3.8 Live ingestion pipeline

1. **Wire → verified event**: `JSON.parse`, then `eventDto` (strict schema plus `verifyEvent`, then frozen copy,
   `events.ts:85-135`). **One event per callback. The live path has no batching or yield.** The HTTP
   reads use `parseEvents`, which verifies in batches of 12 with `yieldToHost()` (a `MessageChannel` post) in
   between (`transport.ts:221-243`, `yield.ts:1-12`). The live path does not use the proof memo
   (`createEventVerifier`, a 2048-entry LRU) that HTTP reads use.
2. **Scope fencing** in `live.ts:812-844` (§3.4). Generation fencing: frames from an old socket are dropped by
   `valid()`.
3. **Session `receive(events, provenance)`** (`session.ts:2319-2443`):
   - `reminders.receive`.
   - Incoming-alert candidates: live phase with a channel id, kind 9/40002/40008, not your own, exactly one
     `h` equal to the provenance channel, not already in `recent`, unread or the local outbox.
   - Membership hints: kind 44100/44101 signed by `relayAuthor` with `#p` = viewer →
     `confirmMembershipHints` (a coalesced roster or channel confirmation over HTTP).
   - `accept(events)` (`session.ts:517-577`): discovery (39000/39002) first, then a visibility filter
     (drops 24200, 20002 and product feedback, and checks access and `e`-target visibility). Then
     `typing.accept`, then the `recent` LRU (4096 events / 8 MiB, keyed by id: this is the
     **dedupe**, `session.ts:249-252`), then reads, unread, writes (`observe` = echo → `seen`),
     profiles, emoji, statuses, channels, and observation listeners. Steps are separated by `accessEpoch` checks.
   - Typing (20002) is accepted only while connected and in the same generation (`session.ts:2365-2373`).
     Constants: 8 s lifetime, 2 s quiet period after a message (`typing.ts:4-6`).
   - Live listeners get a frozen `LiveBatch` (`incoming.ts:15-18`). Incoming listeners get
     `IncomingMessage` (preview ≤4096 chars) for the alert candidates.
4. **Ordering**: there is no ordering buffer. Arrival order is not trusted. Views sort by
   `created_at*1000 + ms tag (0–999)`, then by id ascending (`message-order.ts:11-27`). For replaceable
   records, the later `created_at` wins, and on a tie the lower id wins (`events.ts:139-150`).

### 3.9 Dev broker SSE wrapper (reference only)

`broker-live.ts` sends `POST /api/relay/<c>/stream` with body `{channels, joined, priority, observer,
presenceAuthors, interestRevision}`. The response is `text/event-stream` with header `X-Buzz-Live-ID` (32 hex).
Events: `traffic`, `state`, `established`, `denied`, `recover`, `presence`, `observer`, `archive-*`.
Keepalive `: keepalive` every 15 s. The client heartbeat timeout is **45 s**. Backoff is the same
`500*2**n` with 5 attempts, and here `attempts` *is* reset by `retry()`. Control POSTs `/stream-interests`,
`/stream-priority`, `/stream-observer`, `/stream-presence[-authors]` and `/stream-retry` have a 5 s timeout. A failed
interests or observer control aborts the stream and lets it reconnect. At most 8 streams per broker.
(`broker-live.ts:16-520`; `relay-broker.mjs:1527-1712`.)

### 3.10 Gaps and uncertainties

- The client never handles NOTICE. Server-initiated socket close reasons are not shown to the user.
- `live.ts` never resets `attempts` after a successful AUTH. Five drops over a socket's lifetime, even
  hours apart, put live into `error` until the user presses Retry. This could be a bug. Decide whether to keep
  it in the port.
- There is no reconnect on network change or wake. Recovery after sleep depends on the OS closing the socket,
  or on the server's 3 missed pings (about 60–90 s).
- Duplicate reaction (`accepted:false "duplicate: …"`) maps to **failed** over HTTP but **unknown** over WS.
- Not found: any client-side WS-level ping, any `since`-from-last-event resume, or any use of NIP-11
  `limitation` values at runtime (the constants are hard-coded).

---

## 4. HTTP reads

### 4.1 `POST /query`: wire contract

Route: `buzz:crates/buzz-relay/src/router.rs:327` (`.route("/query", post(api::bridge::query_events))`). The api router has a **1 MiB request body limit** (`buzz:crates/buzz-relay/src/router.rs:407-408`).

**Request**
```
POST {httpOrigin}/query
Authorization: Nostr <base64(JSON of signed kind-27235 event)>
Content-Type: application/json

[ {filter}, {filter}, ... ]          <- a bare JSON array of filter objects. No wrapper object.
```
- Body: `JSON.stringify(filters)`, where `filters` is the reader's canonical filter list (`src/features/relay/transport.ts:1232-1243`, `signedPost` at `:1263-1288`). The relay parses it with `serde_json::from_slice::<Vec<Value>>`. A non-array body returns 400 `invalid filters: ...` (`buzz:crates/buzz-relay/src/api/bridge.rs:1340-1341`).
- `httpOrigin` = an explicit host HTTP base, or the `ws(s)://` relay URL with its scheme changed to `http(s)://`. It has no path, and a trailing `/` is removed (`src/features/relay/transport.ts:206-219`).
- NIP-98 auth event (built by the native host, `src-tauri/src/relay.rs:720-756`):
  ```json
  {"kind":27235,"created_at":<now>,"content":"",
   "tags":[["u","<full request URL>"],["method","POST"],
           ["payload","<sha256 hex of exact body bytes>"],["nonce","<uuid v4>"]]}
  ```
  This is base64 (STANDARD) of the serialized signed event. Use a fresh event for each request. The relay checks: `created_at` within **±60 s** (`buzz:crates/buzz-auth/src/nip98.rs:32`); exactly one `u` tag equal to `{http|https}://{tenant host}/query`, with the scheme taken from the relay's configured `wss://` vs `ws://` (`buzz:crates/buzz-relay/src/api/bridge.rs:335-346`); a matching `method`; a `payload` tag that is **required** in NIP-FI enforce mode (`buzz:crates/buzz-relay/src/api/bridge.rs:1246-1268`, `:138-148`). Replay protection keys on the auth event ID, so a reused auth event gets 401 (`buzz:crates/buzz-relay/src/api/bridge.rs:276-333`). Without auth, a dev-only `X-Pubkey` header fallback exists (`:160-172`). Do not port it.
- The dev browser broker variant (`src/features/relay/transport.ts:1056-1083`) sends `X-Buzz-Read-Priority: foreground|background`, and the broker signs. The native path does **not** send this header.
- Native HTTP client: connect timeout 10 s, total timeout 30 s, no redirects (`src-tauri/src/relay.rs:640-652`). Max response 16 MiB (`:41`). A transport error is never retried (`:762-766`).

**Response**
- 200: **always a flat JSON array of signed Nostr events** (`Json(Value::Array(events))`, `buzz:crates/buzz-relay/src/api/bridge.rs:1813`). There is no envelope, no cursor field, and no `has_more` field. Pagination metadata arrives only as relay-signed overlay events *inside* the array (39006 / 39007 / 39005; see §5b, §5c).
- Errors: `{"error": "<message>"}` (`buzz:crates/buzz-relay/src/api/mod.rs:23-25`). Observed statuses:

| Status | Body `error` (examples) | Source |
|---|---|---|
| 400 | `invalid filters: …`, `too many explicit channels` (>128 total `#h` values), `thread_window cannot mix with other query modes`, `mixed search and non-search filters not supported`, `consistency must be "strong" when present`, `before_id must be a 64-char hex event id`, `before_id requires until to be set`, `top_level requires exactly one #h channel`, `top_level cursor requires both until and before_id, or neither`, `resolve_thread_roots permits at most 100 targets per request`, thread_window parse errors | `buzz:crates/buzz-relay/src/api/bridge.rs:1340-1453,1698,1726-1736,694-731`; `buzz:crates/buzz-relay/src/handlers/req.rs:42` |
| 401 | `missing Nostr auth`, `NIP-98: …`, replay | `buzz:crates/buzz-relay/src/api/bridge.rs:105-175,276-333` |
| 403 | `restricted: p-gated kinds require #p tag matching your pubkey`, `restricted: agent-engram reads …`, `restricted: author-only kinds require authors=[self]`; relay-membership refusal | `buzz:crates/buzz-relay/src/api/bridge.rs:1356-1375,1325-1332` |
| 404 | `buzz:crates/buzz-relay/src/ no community is configured for this host` | `:1234-1242` |
| 429 | `rate-limited: quota exceeded; retry in {N}s` (per-principal `human_api_calls_per_min`, 60 s window) | `:30-52` |
| 500 | `internal server error` | `buzz:crates/buzz-relay/src/api/mod.rs:27-30` |
| 503 | `query timed out` (statement deadline), `rate-limited: shared admission unavailable`, thread-window budget/deadline/authorization-changed | `buzz:crates/buzz-relay/src/api/mod.rs:35-49`; `buzz:crates/buzz-relay/src/api/bridge.rs:53-58`; `buzz:crates/buzz-relay/src/api/bridge/thread_window.rs:51-117` |

- An inaccessible or nonexistent channel is **not** an error. Its filters return no events (empty array), so access denial and "no data" look the same unless the request is a window request (a missing 39006/39007 is the signal).

**Client parsing** (`src/features/relay/transport.ts:1244-1258`, `:221-243`; `src/features/relay/events.ts:83-130`):
1. For a non-2xx response: read at most **4096 bytes** of the error body and map it with `apiFailure()` (`src/features/relay/http-admission.ts:31-113`). Throw `ReadError(kind, message, status, retryAfterMs)`, where `kind = "denied"` for 401/403 and `"unavailable"` otherwise (`src/features/relay/errors.ts:1-24`). The error text is sanitized: only `rate-limited: quota exceeded…` and three broker strings pass through. Everything else becomes `Relay request failed (<status>)`.
2. For 2xx: `JSON.parse` the text. If the result is not an array, fail with `invalid-response`. Verify every element in batches of **12**, yielding between batches. Each element must have a safe-integer `created_at >= 0`, an integer `kind` in 0..65535, `id`/`pubkey` matching 64 hex, `sig` matching 128 hex, a string `content`, and `tags` as an array of string arrays. The hash and Schnorr signature must verify. **A single bad event rejects the whole response.** A per-transport LRU memoizes verified `(id, sig)` pairs (2048 entries) (`events.ts:66-80`).
3. In the reader (`src/features/relay/reader.ts:159-173`): if the total `byteSize(events) > 8 MiB`, fail with `invalid-response`. Then run `verifyThreadWindows()` (§5c.4). The result is frozen.

### 4.2 Filter object: standard and Buzz extension fields

Client type: `src/features/relay/events.ts:16-44`. The relay parses each filter twice: once as `nostr::Filter` (which drops unknown keys), and once as raw `Value` for extension keys (`buzz:crates/buzz-relay/src/api/bridge.rs:386-389`, `:1336-1351`).

Dispatch order inside `/query` (`buzz:crates/buzz-relay/src/api/bridge.rs:1315-1813`). The first match wins for each filter:
1. Artifact kinds (45010/45011) go to `artifact::query` (`:1342`).
2. If **any** filter has `thread_window` (any value except absent/false), **every** filter must be a valid thread window. Otherwise the result is 400 (`:1346,1377-1395`).
3. Read-state snapshot (`:1397`) is out of scope here.
4. Load the accessible channel set, plus an exact `is_member` repair for each requested `#h` (`:1402-1412,1816-1851`).
5. If **any** filter has `search`, all filters must have it, and the request goes to the FTS path (`:1414-1432`).
6. Presence synthesis for the `kinds:[20001]` author filter (`:1434-1438`).
7. For each remaining filter: `resolve_thread_roots`, then `top_level` (channel window), then `feed_types`, then `depth_limit`+single `#e` (legacy thread), then the catch-all NIP-01 query (`:1440-1810`).

| Field | Type | Semantics | Server |
|---|---|---|---|
| `kinds`,`ids`,`authors`,`since`,`until`,`limit`,`#x` | NIP-01 | Standard. Catch-all order is `created_at DESC, id ASC`. The server clamps the limit to 1000 (`buzz:crates/buzz-db/src/store/event.rs:33`). The client caps it at 500. | `buzz:crates/buzz-relay/src/api/bridge.rs:1675-1810` |
| `#h` | `[uuid]` | NIP-29 channel. At most **128** explicit values per request. Inaccessible values are skipped silently. | `buzz:crates/buzz-relay/src/handlers/req.rs:36-42`; `buzz:crates/buzz-relay/src/api/bridge.rs:1351-1353,1685-1689` |
| `before_id` | 64-hex string | Composite keyset cursor with `until`: rows where `created_at < until OR (created_at == until AND id > before_id)`. A malformed value returns 400. Without `until` it returns 400. | `buzz:crates/buzz-relay/src/api/bridge.rs:397-417,1726-1740`; client mirror `src/features/relay/session-window.ts:13-18`, `projection.ts:20-25` |
| `consistency` | `"strong"` only | Pins the read to the **writer** DB pool (read-your-writes). It is absent by default (replica-eligible). Any other value returns 400. It applies **only in the catch-all phase**: window/feed/legacy-thread filters ignore it, and a `thread_window` filter that contains it returns **400** (unknown field). | `buzz:crates/buzz-relay/src/api/bridge.rs:419-469,1691-1699,1763-1776`; `buzz:crates/buzz-core/src/thread_window.rs:85-98` |
| `search` | string | Postgres FTS. Empty is skipped. `limit` defaults to 100 and is capped at 500. `#h` is intersected with accessible channels. Results keep **relevance order** and are deduped across filters. FTS indexes kinds 0, 9, 40002, 45001, 45003. | `buzz:crates/buzz-relay/src/api/bridge.rs:2231-2371` (comment at ~2343) |
| `search_mode` (`searchMode`) | `"prefix"\|"fulltext"` | Defaults to fulltext. | `:539-548` |
| `page` (`search_page`,`searchPage`) | int ≥1 | 1-based page in search. In a **non-search** catch-all, `page>1` gives `OFFSET (page-1)*limit`. | `:550-580,1751-1753` |
| `feed_types` | `[string]` | Ordered list from `"mentions"`, `"needs_action"`, `"activity"` (alias `"agent_activity"`). Unknown types are skipped. Duplicates are collapsed. `limit` defaults to 20 and is capped at 100 in total across types. `since` is honored. Results are deduped by ID. Output is concatenated in type order. | `:526-537,1481-1570` |
| `top_level` | `true` | Channel-window mode (NIP-CW channel mode). See §5b.1. | `:683-865,1463-1478` |
| `include_aux` | bool | In window, legacy-thread, and thread-window modes: append a 2-hop aux closure (hop 1: kinds 5, 7, 9005, 40003 that target rows; hop 2: kinds 5, 9005 that target hop-1 events). | `:586-598,767-806,1641-1670` |
| `include_summaries` | bool | Window mode only: one relay-signed 39005 for each row that has replies. | `:812-840` |
| `thread_window` | `true` | NIP-CW thread mode. Strict field allowlist. See §5c.4. | `buzz:crates/buzz-relay/src/api/bridge/thread_window.rs`; `buzz:crates/buzz-core/src/thread_window.rs` |
| `depth_limit` | u32 | Legacy oldest-first thread traversal when it is present **and** exactly one 64-hex `#e` is present. In thread_window mode the range is 1..100 (default 100). | `buzz:crates/buzz-relay/src/api/bridge.rs:485-489,1572-1673` |
| `thread_cursor` (`threadCursor`), `thread_cursor_id` (`threadCursorId`) | i64, hex | Legacy thread forward cursor: `created_at > c OR (= c AND id > cid)`. | `:491-524` |
| `resolve_thread_roots` | `true` | With `kinds:[39005]`, one accessible `#h`, and 1–100 `ids` of *reply* events (soft-deleted ones included), it returns one fresh 39005 for each owning root, read from the writer. It has **no client caller**. | `:1440-1460`; `buzz:crates/buzz-relay/src/api/bridge/thread_roots.rs`; `buzz:docs/nips/NIP-CW.md:389-407` |
| `#buzz-channel` | `[string]` (1) | Custom-tag match. It has no client caller in buzz-app. | `:471-479,1708-1710` |

Client usage (grep):
- `consistency:"strong"` is used by discovery, lifecycle, details, member-admin, DMs, work-sessions, and templates (`ChannelReadOptions`, `src/features/relay/contracts.ts:163-164`).
- `search` is used by `src/app/shell/useSearchMessages.ts:142-190` with `{kinds:[9,40002,40008], search, search_mode:"prefix", limit:20, #h?, authors?, since?, until?}`, and by the profile search `{kinds:[0], search, search_mode:"prefix", limit:40}`. A `page` field is sent by member/DM people search (`direct-messages.ts:43-47`, `channel-members/search.ts:33`).
- **`feed_types` is typed but has no caller in buzz-app** (only `events.ts:39`, `projection.ts`, `session.ts:1928` guard it).
- Global message search (`kinds ⊆ {9,40002,40008}`, no `#h`) has the session resolve the hit channels (≤128 IDs) and drop hits whose channel is not readable. Relay order is kept (`src/features/relay/session.ts:462-500`).
- `session.observe()` rejects any filter with `search`/`feed_types`, because ranked results cannot be merged locally (`session.ts:1922-1933`; `projection.ts:101-103`).

### 4.3 Read scheduler (client)

Layering: `RelayReader` (session-owned: dedup, priority, deadline) → `ReadTransport.query` → `signedPost` → per-principal **API admission lane** (`http-admission.ts`) → host `fetch`/Tauri `relay_http`.

**Reader** (`src/features/relay/reader.ts`):
- Validation (`:192-237`), applied before queueing:
  - **1–4 filters** per read (a workflow-definition batch is exempt).
  - Each filter needs non-empty valid `kinds` (0..65535) **or** `ids` with no `kinds`.
  - `limit` must be an integer **1..500**.
  - The canonical key is at most **64 KiB** UTF-8.
- Canonical key (= dedup identity): drop undefined fields, sort keys, and dedup+sort the arrays of `kinds`, `authors`, `ids`, `#*` (`:198-232`). **The canonicalized filters are what is sent on the wire** (`:238`).
- Coalescing: identical keys share one job. Each caller is a "consumer". If one consumer cancels, only it rejects. The job aborts when the last consumer leaves (`:247-298`). `fresh:true` adds `:fresh:<seq>` to the key so the read never joins an in-flight job (used by write preflights and search) (`:246`).
- Capacity: at most **128** pending distinct jobs. More gives `ReadError("unavailable","Too many pending relay reads")` (`:249-252`).
- Deadline: **10 000 ms from enqueue**, queue time included. On expiry: `ReadError("unavailable","Relay read timed out")` (`:49,264-272`).
- Slots: **3** running jobs. **At most 1 background** job running at a time. Foreground jobs are picked first; FIFO otherwise (`:127-137`). A later foreground consumer of a queued job promotes it (`:275-276`). `promote(predicate)` promotes queued jobs without adding a consumer or resetting the deadline (`:312-316`). `invalidate(predicate)` cancels matching jobs (`:317-327`).
- Retries: **none** in the reader or transport. 429/5xx surface as `ReadError` with `status` and `retryAfterMs`. Callers decide what to do. For example, head catch-up stores `retryAt` and defers all queued catch-ups (`src/features/relay/session.ts:2298-2311`). Directory reads retry once on an access-epoch invalidation (`docs/relay-queries.md:440-446`).
- After a fetch rejection, wait one macrotask (`yieldToHost`) before pumping again. Page `beforeunload`/`pagehide` suspends admission, and `pageshow`/next animation frame resumes it (`:69-112`). This is browser-specific. A native port can drop it.

**API admission lane** (`src/features/relay/http-admission.ts:115-264`). There is one lane per `(httpOrigin, viewer)` principal and at most **64** principals (`src/features/relay/host-admission.ts:6-39`):
- **6** requests in flight, awaiting response headers (`:148`). Foreground first, then FIFO. Queue cap **128**, else `ApiCapacity` (`:204`). `prepare()` (async signing) has its own cap of 128 (`:173-182`).
- Pause: only a relay **429 whose body matches `^rate-limited: quota exceeded(;|$)`** pauses the lane. The duration is `(N+1)*1000 ms` from `retry in Ns`. If the hint is missing or unparsable, the client assumes 60 s. The maximum is 86 400 s (`:34-51`). A broker 429 `{paused:true,sent:false,retryAfterMs}` is also recognized (`:52-66`). While the lane is paused, new and queued requests reject immediately with `ApiPaused` (status 429, `retryAfterMs`) and do not wait out their 10 s deadline (`:139-147,197-204`).
- There is no pacing, no token bucket, and no inter-request delay. The pause applies to publishes as well (`signedPost` shares the lane, `transport.ts:1211-1230`).
- Presence lane: one lossy request per 5 s that never takes a normal slot (`:157-169`, `transport.ts:1133-1172`).

**Budget helpers** (`src/features/relay/budget.ts`): `byteSize(v)` = UTF-8 length of `JSON.stringify(v)` (`:4-5`). `ByteLru(maxEntries,maxBytes)` evicts the least recently used entry (`:24-71`). These are used for every retention cap below.

**Port notes.** Keep: a bare-array body, a fresh NIP-98 event per request with a payload hash, the 3/1-background reader slots, the 6-slot lane, the 10 s deadline including queue time, the 1–4 filter / 500 limit validation, identical-read coalescing, and quota-pause semantics. Partition by `(origin, viewer)`.

---

## 5. Feature data flows

Each subsection below covers one UI feature: the exact queries, events and state it uses.
In the reference app, all of them share one per-community **session** (`src/features/relay/session.ts`).
That session owns:
- the reader (§4.3) and the live socket (§3);
- the outbox (§6);
- the verified `recent` event LRU (4096 events / 8 MiB, keyed by id);
- the `accessEpoch` fence, which drops late results after a revocation (§8.5).

Every verified batch, whether it came from an HTTP read or from live traffic, goes through
`session.accept()` (`src/features/relay/session.ts:517-577`). Its steps, in this order:
1. Discovery (39000/39002).
2. The visibility filter.
3. Typing.
4. `recent` dedupe.
5. Read state.
6. Unread.
7. Outbox echo (`seen`).
8. Profiles, emoji, statuses, channels, threads.

Rust implementation guidance: give each feature store an `accept(&[VerifiedEvent])` method, and have
the session call the stores in this order.

Common write receipt: every write is `POST /events` (§3.7). The body is
`{"event_id":hex,"accepted":bool,"message":string}`. Command kinds (41010, 41011, 9007, …)
return their result in `message` as `"response:<json>"`, for example
`response:{"channel_id":"<uuid>","created":true}`. Other commands return `"{}"`
(`buzz:crates/buzz-relay/src/handlers/ingest.rs:428`,
`buzz:crates/buzz-relay/src/handlers/command_executor.rs:294-426`).

## 5a. Sidebar: channel list, membership, sections, preferences

### 5a.1 Kinds involved (summary)

| Kind | Author | Purpose | Key tags |
|---|---|---|---|
| 39000 | relay | channel metadata (addressable, `d`=channel UUID) | `d,name,about?,public|private,hidden?,p*(DM only),closed,t,topic?,purpose?,archived?,ttl?,ttl_deadline?` |
| 39001 | relay | admins (owner/admin only) | `d`, `["p",pk,role]` |
| 39002 | relay | full roster | `d`, `["p",pk,"",role]` |
| 30622 | relay | per-viewer hidden-DM set (NIP-DV) | `d`=viewer, `p`=viewer, `h`* |
| 30078 | viewer | sidebar prefs, NIP-44 self-encrypted | `["d",coord],["t",coord]` |
| 40099 | relay | system message (membership activity) | `h` |
| 44100/44101 | relay | member added/removed notification (hint only) | `p`=target, `h` |
| 9, 40002, 40008, 45001, 45003 | any | "activity" kinds for Recent sort | `h` |

### 5a.2 Channel discovery / membership determination

Source: `src/features/relay/store.ts:1015-1210`, `src/features/relay/discovery.ts:24-296`.

1. **Roster scan** (pages of `DISCOVERY_LIMIT = 500`, `store.ts:80`):
   ```json
   {"kinds":[39002],"#p":[VIEWER],"limit":500, "consistency":"strong"?,
    "until":<cursor.created_at>, "before_id":<cursor.id>}
   ```
   (`store.ts:1053-1063`; `until`/`before_id` only after the first page; `consistency:"strong"`
   only when a strong re-list is owed.) Keep events where `kind==39002 && pubkey==relayAuthor &&
   has ["p",viewer] && d`. Sort newest-first `(created_at desc, id asc)`; cursor = last kept.
   Page is "short" (complete) when `rosters.length < 500`. Throw `invalid-response` if the
   cursor does not advance. Max pages = `capacity/500` (capacity 1024 => 3 pages).
2. **Omitted-roster confirmation** (only after multi-page scans): previously authorized ids not
   seen are re-read in batches of `DISCOVERY_CONFIRM_LIMIT = 128` (`store.ts:91,1129-1170`):
   `{"kinds":[39002],"authors":[relay],"#d":[...batch],"#p":[viewer],"limit":batch+1}`.
   Absent from both => access lost (denied).
3. **Metadata fetch** for authorized ids lacking 39000 (`store.ts:1179-1207`):
   `{"kinds":[39000],"#d":[...up to 500 ids],"limit":500}` (no `authors` filter; author
   checked on accept).
4. **DiscoveryState** (`discovery.ts:24`): retains latest (`newer()`) 39000 and 39002 per `d`,
   capacity 1024 each; overflow increments `overflowRevision` and disables completeness.
   - `authorized(id)` = not denied && roster exists && roster has `["p",viewer]` (`:168`).
   - `canAccess(id)` = not denied/suspended && (viewer in roster || open metadata ||
     (no roster && no metadata && scan not complete)) (`:145`).
   - Roster version that previously contained the viewer and a newer one that doesn't =>
     `denied` (`:65-73`). A complete scan that omits a previously-member roster => denied
     (`retain`, `:110-129`). Denials never evict back to "unknown"; when denied set reaches
     capacity, `denyAll()` (`:81-92`).
   - `openMetadata(ev)` = has `public` tag && no `private`/`hidden` && `t != "dm"`
     (`discovery.ts:15-21`). Open metadata grants **reading** (readOnly preview), never membership.
   - `canParticipate(id)` (can write) = canAccess && not cached-from-disk && viewer in roster (`:158`).
   - `channels()` (= sidebar roster) = authorized ids only, sorted by
     `name.localeCompare || id.localeCompare` (`:287-295`).
5. **Live hints**: subscribe `{"kinds":[44100,44101],"#p":[viewer]}` (`src/features/relay/live.ts:587`).
   A relay-signed 44100/44101 with `p=viewer` only triggers a roster re-read; 44100 with exactly
   one `h` queues an exact `#d` resolve of that channel, anything else triggers a full strong
   roster refresh (`session.ts:2117-2140, 2340-2356`). Notification content:
   `{"type":"member_added"|"member_removed","channel_id":uuid,"actor":hex}` tags `[["p",target],["h",uuid]]`
   (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:980-1063`; kinds `buzz:crates/buzz-core/src/kind.rs:534,538`).
6. **Cached roster**: snapshot restored from disk is display-only (`cached:true`, `readOnly`),
   no writes until a fresh network 39002 confirms the same-or-newer version (`discovery.ts:55-62,215-218`).

Server: 39000/39001/39002 are stored **channel-scoped**, so private-channel metadata/rosters are
only readable by members; live global subscriptions to `{kinds:[39000]}` do NOT receive them
(`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1181-1188`). They are re-emitted on create, metadata edit, any membership
change (`emit_group_discovery_events`, `buzz:crates/buzz-relay/src/handlers/side_effects.rs:1189-1302`). Relay forces each new
version's `created_at > previous` (`:1065-1097`).

### 5a.3 39000 metadata shape (exact, as emitted)

`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1207-1265`. Content is `""`. Tag order:
```json
[["d","<uuid>"],
 ["name","<name>"],
 ["about","<desc>"],              // only if description non-empty
 ["private"] | ["public"],        // exactly one; single-element tag
 ["hidden"],                      // DM only
 ["p","<hex>"], ...               // DM only: every participant (2-element)
 ["closed"],                      // always
 ["t","stream"|"forum"|"dm"|"workflow"],
 ["topic","..."], ["purpose","..."],   // optional, non-empty
 ["archived","true"],             // only when archived; absent when active
 ["ttl","<seconds>"],             // optional
 ["ttl_deadline","<RFC3339>"]]    // optional
```
There is **no `picture` tag** and no content JSON in server output. Client `metadataName()` still
prefers `content.name` if content parses as an object with string `name` (`discovery.ts:8-12`).

Client projection to `ChannelSummary` (`discovery.ts:200-270`, type `src/features/relay/contracts.ts:16-45`):
- `visibility` = `channelVisibility(ev)`: exactly one `public`/`private` single-element tag, else
  `undefined` (unknown, never implicitly public) (`channel-details-protocol.ts:26-36`).
- `channelType` = `t` if in {stream, forum, dm}; `"session"` if `t=stream && private &&
  about` parses as session metadata (`about == "Buzz session (buzz.sessions/v1)"` or that +
  `"\nparent:<uuid>"`; `src-tauri/src/relay/channel_writes.rs:629-635`). `workflow` => undefined.
- `description` = `about ?? ""` (omitted for sessions); `hidden`, `private`, `archived` (`archived=="true"`).
- `members` = sorted unique 64-hex `p` values from 39002; DM `participants` = members minus viewer
  (empty => "Notes to self").
- `readOnly` when not authorized (public preview) or cached; `name` fallback `id.slice(0,8)`.

### 5a.4 Channel types and sidebar sections

`src/bundled/channels/sidebar-sections.ts:13-117`. Input = authorized roster after DM labelling,
minus Me-placed channels (`ChannelSidebar.tsx:293-301`; Me ids from kit record, see 5a.8).
- `active` = `!archived && (!hidden || channelType=="dm")` (archived channels never shown;
  reachable via search only).
- `streams` = active where type not dm/forum (includes stream, session, unknown).
- `dms` = active dm and not in hidden-DM set.
- `movable` = streams + dms (forums are never movable/starrable into groups).
Section order and contents:
1. `starred` "Starred" (icon ★): movable ∩ starred; sort key `"starred"`. Rendered only if non-empty.
2. one per saved group `group:<sectionId>` (title=name, icon): movable, not starred, assignment==group; sort key `section:<id>`.
3. `channels` "Channels": streams not starred and not assigned to a *live* group; always rendered.
4. `forums` "Forums": all active forums (ignores stars/groups); only if non-empty.
5. `dms` "Direct messages": dms not starred, not assigned; always rendered (header "+" opens New message).

Sorting within a section (`sidebar-sections.ts:34-55`): if `sort[key]=="recent"`: by
`lastActivityAt` desc; channels with activity before those without; ties/none fall through to
name. Default (alpha): case-insensitive `name.toLowerCase()` compare, then `id.localeCompare`.
DM names come from labels (5a.9), so DMs sort by resolved participant names.

`lastActivityAt` (`src/features/relay/channel-activity.ts:1-145`, reader `native.ts:810-829`):
per channel, one filter `{"kinds":[9,40002,40008,45001,45003],"#h":[id],"limit":1}`, batched 128
channels per query, 10 s timeout, background priority. Event must have exactly one `h`. Live
events only advance values; an authoritative refresh may clear values unchanged during the read.
Only needed if any sort is `recent` (startup gate, `useSidebarStartup.ts:69-74`).

Startup reveal (`src/bundled/channels/useSidebarStartup.ts:11,63-78`): reveal when roster is
ready/error AND (all of: prefs loaded/error/unsupported, live roster not pending, activity ready
if any `recent`, `unread.ensure()` settled) OR `SIDEBAR_REVEAL_BUDGET_MS = 1500` elapsed OR any
channel is restored from cache.

### 5a.5 Hidden channels / DMs in the sidebar

Two independent DM-hiding mechanisms; sidebar hides the **union**
(`src/features/channel-navigation/ChannelSidebar.tsx:525-530`):
1. **Relay hide (41012 -> 30622)**: see 5e.5. `lifecycle.refreshVisibility()` runs whenever the
   roster list becomes ready / `asOf` changes (`ChannelSidebar.tsx:232-235`).
2. **Local "Remove from Messages"** (`src/bundled/channels/useHiddenDms.ts:1-237`,
   `ChannelSidebar.tsx:776-815`): localStorage view key `"hidden-dms"` in scope
   (storage key `buzz-view.v1:` + `JSON.stringify([scope,"hidden-dms"])`, `src/shared/view-state.ts:44-73`).
   Value: `[{id, baseline?: {id, createdAt}|null, knownIds?: string[]}]`.
   On hide: baseline = current unread `latestMessage` for the channel. Auto-unhide when:
   - DM opened via `directMessages.open` (subscribeOpened);
   - any incoming live message in that channel (`subscribeIncoming`);
   - viewer sends kind 9/40002/40008 with that single `h` (only if not re-hidden meanwhile);
   - unread head newer than baseline (`createdAt >` or equal with smaller id);
   - history probe `{"kinds":[9,40002,40008],"#h":[id],"limit": knownIds?50:100}` returns an id
     not in `knownIds` (first probe just records `knownIds`); 3 attempts, backoff 500·2^n ms.
`hidden` 39000 tag (NIP-29) hides non-DM channels entirely (`sidebar-sections.ts:20`).

### 5a.6 Sidebar preferences: kind 30078, NIP-44 to self

Coordinates (exact `d` values, `src/features/relay/sidebar-preferences.ts:5-10`,
`src-tauri/src/identity.rs:566-571`): `channel-sections`, `channel-stars`, `channel-mutes`,
`channel-sort`. **No community/relay scoping in the `d`**; one record set per pubkey per relay.

Event shape (signer `src-tauri/src/identity.rs:708-746`; admission `:693-705`):
```json
{"kind":30078,"created_at":max(now, prevHead.created_at+1),
 "tags":[["d","channel-sort"],["t","channel-sort"]],
 "content":"<nip44 v2 encrypt(sk, own pk, JSON.stringify(payload))>"}
```
Tags must be exactly `[["d",c],["t",c]]`. Plaintext ≤ 128 KiB; signed event ≤ 192 KiB.
Decode accepts ≤ 4 events, ≤ 768 KiB total, must verify sig, `pubkey==self`, exactly one `d`,
known coordinate, no duplicate coordinate (`identity.rs:635-688`).

Read (`sidebar-preferences.ts:199-238`): one request with four filters
`{"kinds":[30078],"authors":[viewer],"#d":[coord],"limit":1}`, foreground priority; session
wraps it with a 10 s timeout (`src/features/relay/session.ts:1186-1197`). Write head read uses the
same filter + `"consistency":"strong"` (`native-sidebar.ts:58-74`).

**Payload schemas** (validated in `identity.rs:338-440` and `sidebar-preferences.ts:78-197`):

`channel-sections`:
```json
{"version":1,
 "sections":[{"id":"<≤256>","name":"<≤256, non-blank>","order":<finite number>,"icon":"<≤128>"?}],
 "assignments":{"<channelId>":"<sectionId>"},
 "meta":{"v":1,
   "s":{"<sectionId>":{"name":R,"icon":R,"order":R,"live":R}},
   "a":{"<channelId>":R}}}
```
`channel-sort`:
```json
{"version":1,"groups":{"<groupKey>":"alpha"|"recent"},
 "meta":{"v":1,"g":{"<groupKey>":R}}}
```
groupKey ∈ `starred|channels|forums|dms|section:<sectionId>` (≤264 chars).
`channel-stars` / `channel-mutes` (no `meta`):
```json
{"version":1,"channels":{"<channelId>":{"starred"|"muted":bool,"updatedAt":<ms ≥0>}}}
```
Limits: sections ≤100, assignments ≤1000, sort groups ≤104, stars ≤500, mutes ≤500; text fields
non-blank, length counted in UTF-16 units.

**Register `R`** (`src/features/relay/sidebar-registers.ts:2,22-37`): `[version, device, value]`,
`version` = non-negative safe-integer ms, `device` = 16 lowercase hex. Values:
`name` string; `icon` string|null; `order` safe int; `live` bool; assignment `a` = sectionId|null;
sort `g` = `"alpha"|"recent"|null`. Unknown keys in `meta` or in a section node => fail closed.

**Projection (meta is authoritative)** (`sidebar-registers.ts:105-156`): when `meta` present,
top-level `sections/assignments/groups` are recomputed from it: sections with `live===true` and a
string name, sorted by `(order, id)` and re-numbered densely 0..n; assignments only pointing at
live sections; sort groups with non-null value. Writers must emit legacy fields equal to this
projection (`identity.rs:428-438`, "Sidebar projection disagrees with metadata").

**Edit / merge rules** (`sidebar-registers.ts:157-271`, `sidebar-edits.ts`):
- Meta-less head => import legacy fields as registers at `createdAt*1000` with device `"0"*16`.
- Edits write only the requested leaves; all other registers and tombstones are preserved.
- New register version = `max(Date.now(), maxVersionInTree+1)`; device = random 8 bytes hex,
  per process (in-memory).
- No-op if every write already equals current value (no publish). Removing an assignment that
  isn't projected is a no-op.
- Assign: `a[channelId] = sectionId|null`. Create-and-assign also writes
  `s[id].{name,icon:null,order:nextOrder,live:true}`; `nextOrder` = max live canonical order + 1.
  Section id must be a UUID (`sidebar-edits.ts:35-47`). Retrying with same id but different
  name => error.
- Remove section: `s[id].live=false` plus `a[ch]=null` for each projected assignment to it.
  Store first resets that section's sort to alpha (writes `g["section:<id>"]=null`)
  (`sidebar-preferences-store.ts:370-392`).
- Sort: `g[group] = mode=="alpha" ? null : mode`. Validation: mode alpha|recent; group fixed key
  or `section:<id>` for a live section id (`sidebar-edits.ts:53-66`).
- Star/mute toggles: `channels[id] = {...prev, starred|muted: bool, updatedAt: max(now, prev+1)}`.
  Unstar of absent entry is a no-op; **unmute always persists `muted:false` tombstone**.
**Conflict model**: whole-record last-write-wins per coordinate. Mutation =
strong head read -> edit -> sign -> publish -> strong readback; readback must show the requested
intent, else error `"Sidebar … changed on another device; reload and try again"`
(`native-sidebar.ts:108-147,148-297`). No cross-device merge beyond preserving registers present
at the head read (`docs/channels.md:207-255`).

**Store / optimism** (`src/features/relay/sidebar-preferences-store.ts`):
- One serialized write queue for move/star/sort/mute per session (`:60`). **No debounce timers**
  anywhere in sidebar prefs.
- Move to group = assignment write then star write `{starred:false}`; move to Starred = star write
  `{starred:true}` only (assignment untouched). Pending moves/sorts overlay the confirmed snapshot;
  failure records `failedMoves[channelId]` / `failedSorts[group]` with Retry/Dismiss.
- Moves gated until a successful full read (`writable()`).
- Mute is NOT optimistic at store level (notification policy uses confirmed mutes); UI-only
  optimism in `src/bundled/channels/useOptimisticMute.ts:1-72` (latest click wins, error retained).
- Startup cache: last confirmed prefs saved via persistence `writeStartup` with `savedAt`; restored
  as display-only (`cached:true`) if `0 ≤ age ≤ 86_400_000` ms (`:420-462`). Writes disabled while cached.

### 5a.7 Mute

Mute = `channel-mutes` entry `muted:true`. `muted` list consumed by notifications/unread (not
specified here). Mute never hides the row. Same write path as 5a.6.

### 5a.8 Me groups (channel kit, kind 30078)

`src/features/relay/me-preferences.ts`, `src-tauri/src/relay/kit.rs:4-6,109-260`.
Personal groups (a `groups:personal` kit record replacing the Messages sidebar sections) were
retired in buzz-app #988: the sidebar sections always come from `channel-sections`. Saved
`buzz-channel-kit-v1:<enc>:groups:personal` records are skipped before decode so they never
reject the catalog. The only groups record left is Me's:
- `d = "buzz-me-kit-v1:<enc>:groups:me"`, `t="buzz-me-kit-v1"`.
- Content NIP-44 to self of `{"version":1,"community":origin,"deleted":bool,"value":Groups}`
  (≤16 KiB), `Groups = {type:"groups", id:"me", groups:[{id,name,defaultTemplateId}],
  assignments:{channelId:groupId}, channels:string[]}` (`src/features/channel-templates/model.ts:33-40`).
- Me `channels[]` = channels pulled out of the main sidebar into "Me" (`src/features/sessions/personal.ts:6-22`).
Full kit spec belongs to the templates section.

### 5a.9 Labels

`src/bundled/channels/useChannelLabels.ts:1-77`: filter `!archived && (!hidden || dm)`; DM name =
participants' resolved names joined `", "` (fallback `pubkey.slice(0,10)`), or `"Notes to self"`
if no other participants. Profiles fetched via shared directory (≤1024 ids, background), re-ensured
when roster membership changes (`docs/channels.md:1043-1064`).

### 5a.10 Local view state (not synced)

`src/bundled/channels/useSidebarView.ts:4-6,27,188,225`: key `"channel-sidebar"` =
`{collapsed: string[] (section keys), scrollTop, width (220..520, default 260), location?}`.
Saved on `pagehide`/unmount and on width change.

### 5a.11 Unread & working-agent inputs (reference only)

- Unread per row: `session.unread.snapshot({kind:"channel", channelId})` (fields incl.
  `latestMessage {id, createdAt}`); spec in `docs/unread.md`.
- Working-agent indicator: agents with a `working` turn or a non-thread typing entry in the
  channel (`src/bundled/channels/working-agents.ts:7-21`).
- `messagePreview()` skips membership rows (`membership.ts:53-63`); sidebar doesn't render previews.

---

## 5b. Channel timeline

### 5b.1 Head and older pages: query shape

Builder: `src/features/relay/window.ts:69-86`. Row kinds: `CHANNEL_ROW_KINDS = [9, 40002, 40008, 40099]` (`src/features/relay/membership.ts:6-8`).
```json
[{ "kinds":[9,40002,40008,40099], "#h":["<channel uuid>"], "limit":20,
   "top_level":true, "include_aux":true, "include_summaries":true }]
```
- Page size: `WINDOW_PAGE_SIZE = 20` for both the head and older pages (the builder clamps to 1..200). The relay default is 50 and its max is 200 (`buzz:crates/buzz-relay/src/api/bridge.rs:582-583,733-737`).
- Older page: add `"until": next_cursor.created_at, "before_id": next_cursor.id`. Both values are echoed **verbatim** from the previous page's 39006.
- Kinds: 9 = chat message, 40002 = agent envelope (content is JSON `{content}`), 40008 = diff, 40099 = relay-signed membership activity row.

**Server processing** (`buzz:crates/buzz-relay/src/api/bridge.rs:683-865`; `buzz:docs/nips/NIP-CW.md:59-176`):
1. The filter needs exactly one `#h` UUID, else 400. An inaccessible channel returns `Ok` with **nothing appended**: no rows and no 39006.
2. Rows are top-level (depth 0, or depth 1 with `["broadcast","1"]`), not deleted, matching `kinds`, in order `created_at DESC, id ASC`. The cursor filter is `created_at < until OR (= until AND id > before_id)`. The relay probes `limit+1` rows to set `has_more`. `next_cursor` = the scan position of the last retained candidate. That event **may not be in the response**.
3. The response array order is: rows (keyset order), then aux closure (if `include_aux`), then 39005s (if `include_summaries`), then **exactly one 39006**. Aux hops page in units of 1000 with up to 64 pages per hop (`:600-681`). NIP-CW's text says "1000 per hop", but the code drains up to 64k. Clients must partition events by kind, not by position.
4. 39006 (relay-signed):
   ```json
   {"kind":39006,"pubkey":"<relay>","tags":[["d","<chan>:head" | "<chan>:<until>:<before_id>"],["h","<chan>"]],
    "content":"{\"has_more\":true,\"next_cursor\":{\"created_at\":1751499000,\"id\":\"<hex>\"}}"}
   ```
   (`buzz:crates/buzz-relay/src/api/bridge.rs:842-863`). `next_cursor == null ⇔ has_more == false`.

**Client validation** `parseWindow()` (`src/features/relay/window.ts:89-142`). Any failure throws, and the page is discarded:
- Exactly one event with `kind==39006`, `pubkey==relayAuthor`, tag `h==channelId`, and tag `d==` the expected suffix (`<chan>:head` or `<chan>:<createdAt>:<eventId>` of the cursor sent).
- The content is a JSON object with boolean `has_more`. `next_cursor` is null or `{created_at: safe int ≥0, id: 64 hex}`.
- The cursor must advance: `next.created_at < cur.created_at`, or equal with `next.id > cur.id`.
- `has_more === (next_cursor !== null)`.
- Returns `{events: all non-39006 events, hasMore, cursor}`.

**Store** (`src/features/relay/store.ts`):
- `readPage` (`:513-536`): session-type channels use `readSessionWindow` (below). All others use `windowFilter` + `parseWindow`.
- `loadPage(state, cursor)` (`:610-684`):
  - Head (`cursor=null`) **replaces** `state.events`. Older pages union by ID into `state.events`. The whole union is folded.
  - If `rows > maxHistoryRows (2400)` or `byteSize > maxHistoryBytes (8 MiB)`, the page is not applied and `historyLimited:true` is set.
  - On success: `hasMore = page.hasMore && page.cursor !== null`, `atHead = !cursor`, `freshness:"verified"`.
  - On `denied`, the store calls `denyChannel()`. On other errors: with existing rows, it keeps them and sets `error` and `freshness:"cached"`. Otherwise `status:"error"`.
- `loadOlder` (`:2054-2072`) is a no-op unless `status=="ready" && hasMore && !loadingOlder && !historyLimited && cursor && !controller`.
- `ensure` (`:1940-1993`) and `refresh` (`:2041-2053`): `FRESH_FOR = 60 000 ms` head lease (`:99`). A retained head younger than 60 s that is not `cached` is reused without a read. On revisit, an older or cached head is revalidated only while the reader is still at head.
- Prepared heads: `requestHead()` (`:537-609`) caches `{rows, cursor, hasMore, events, savedAt}` in `heads = ByteLru(64, 4 MiB)` (`:141-142,179`). Speculative hover prepare (`prepare`, `:1994-2040`) runs one head at a time at foreground priority.
- Persistence (`:450-470,685-830`): heads (not session channels) go to IndexedDB, 64 records / 8 MiB, with a 24 h expiry. On restore, every event is re-verified in batches of 8, then `transport.visible()`, then `parseWindow(...head)`, and the head is shown with `freshness:"cached"`.

**Session-type channels** (`discovery.isSession(id)`) use flat paging with no signed bounds (`src/features/relay/session-window.ts:26-92`):
- Rows: `{kinds:[9,40002,40008,40099], "#h":[id], limit:20, until?, before_id?}`. The client checks that rows advance strictly past the cursor.
- Then overlays are fetched by explicit `#e` paging: `{kinds:[5,7,9005,40003], "#e":rowIds, limit:500, until?, before_id?}`, repeated until an empty page. Then `{kinds:[5,9005], "#e":<ids of 7/40003 found>}` the same way. The cap is 2000 events / 4 MiB.
- `hasMore = !!lastRow`: it keeps offering older history until an empty page. `cursor` = the last row's `(created_at,id)`.
- Fold uses `includeReplies:true` for sessions, so replies are rows (`store.ts:129-131`).

### 5b.2 Ordering

- Wire/page order: `created_at DESC, id ASC` (cursor math, retention trimming).
- **Rendered order** `compareMessages` (`src/features/relay/message-order.ts:16-25`): `createdAtMs` ascending, then `id` ascending (JS `localeCompare`; for lowercase hex use a byte compare). Here `createdAtMs = created_at*1000 + ms`, and `ms` comes from an optional `["ms","<0..999>"]` tag (regex `^(0|[1-9]\d{0,2})$`, otherwise 0) (`:10-14`). Senders emit the `ms` tag from `MessageClock.next()` (`:32-58`). The clock tracks a per-channel high-water mark: other authors' evidence can push it at most 5 s ahead, and own sends stay strictly increasing up to a 60 s lead.

### 5b.3 Live updates and merge

Live events come from the WS subscription (another section owns it). The channel REQ kinds are `[9,40002,40008,45001,45003,40099,40100,40003,5,9005,7,39000,39002,39005,20002]` (`src/features/relay/live.ts:200-203`). Verified events go to `store.accept(events)` (`src/features/relay/store.ts:2135-2253`):
1. **Tails** (for channels without an open window and for previews): newest **256** events per channel by `created_at DESC, id ASC`, in `ByteLru(64 channels, 4 MiB)` (`:181-184,2145-2169`).
2. For each open window: collect incoming events with kind in `[9,40002,40008,40099,40003,5,9005,7,39005]` whose `h == channelId` **or** that `e`-reference an ID already in the window or in the incoming set. This is a transitive fixpoint (`:2170-2199`). Append them to `state.events` and to `state.traffic`.
3. Overflow (`:2204-2235`): when non-local row count > 2400 or bytes > 8 MiB, keep the newest 2400 rows, plus all local outbox IDs, plus aux events whose `e` targets a kept ID. Then drop the oldest quarter repeatedly until under the byte budget. Set `historyLimited:true`. New live traffic keeps arriving.
4. `setWindow()` (`:267-320`): `combined = state.events ∪ state.traffic ∪ local outbox ops` whose `h == channel` or whose `e` targets a combined ID. Failed non-content ops (edits, reactions) are excluded, and failed content stays visible for retry. Then `projection.reconcile(combined, ops)`.
- **Live events never change cursors.** Fetched pages stay immutable. Live rows merge at fold time (NIP-CW client rule 4, `buzz:docs/nips/NIP-CW.md:174`).

**Incremental fold** `MessageProjection.reconcile` (`src/features/relay/message-projection.ts:13-169`):
- Index: `overlays[targetId] = Set<overlayId>`, built from every non-row event's `e` tags.
- Affected set: row IDs that were added or removed, plus `e` targets of added or removed overlays. This walks up overlay parents to depth **2**, so a delete-of-reaction re-folds the message.
- For each affected row: `foldMessages(channel, relay, [row, ...overlays(row) ∪ children(overlays, depth 1)])`.
- Delivery-status-only changes patch the row without re-folding. Rows are sorted with `compareMessages`.
- `shareMessageRows` (`src/features/relay/row-identity.ts:1-102`) reuses the previous immutable row object when every field is deep-equal. Snapshots therefore keep identity, which is a UI diffing contract.

**Finite-read/local merge for plugin views** (`src/features/relay/projection.ts:1-104`): union by event ID. A local op overrides delivery and error fields only. Order is `created_at DESC, id ASC` except for search/feed. Retained evidence is capped at 2000 events / 8 MiB, keeping the newest first.

### 5b.4 Gap fill after reconnect

- On stream loss: `staleHeads()` marks all heads and windows `freshness:"cached"` (`store.ts:2384-2389`).
- After live establishment, the session queues a **catch-up** job for each retained window (`src/features/relay/session.ts:2226-2318`):
  - At most 2 run concurrently, and only the selected channel may use the second slot.
  - `requests.invalidate()` first cancels any head read that started before establishment. Its predicate is `#h` includes the channel, `top_level`, no `until`, no `depth_limit`, and kinds include 9 (`:2228-2237,2291`).
  - On a `ReadError` with `retryAfterMs`, it sets `retryAt` and fails every queued job with the same error (`:2298-2311`).
- `store.catchUp(id)` (`store.ts:2392-2432`) = `requestHead()` (a fresh head page of 20), then `accept(head.events)`. **It merges and does not replace.** `cursor` and `hasMore` are updated only if the reader is still `atHead`. Older pages and their cursor are kept.
- Unopened channels get no eager head read. Their next `ensure()` does it.
- **Uncertainty or gap:** if more than 20 top-level rows arrived while disconnected and the reader had paged older, a hole can remain between the new head and the retained rows. Live WS replay (≤500 per channel since route start minus 300 s, `live.ts:19,430,596-599`) may cover it. The code does not prove continuity, and the docs say so (`docs/relay-queries.md:749-759`).

### 5b.5 Folding rows: `foldMessages`

`src/features/relay/fold.ts:248-430`. Input: one window's events. Output: rows in ascending `compareMessages` order. Each step is applied per row event:

- **Row selection** (`:285-296`):
  - Kind in `CHANNEL_ROW_KINDS`, and has tag `["h", channelId]`.
  - Unless `includeReplies`: skip if any `e` tag has `tag[3]=="reply"`, unless `["broadcast","1"]` is present.
  - Skip if deleted (rules below).
- **40099 membership** (`:297-317`): `membershipChange(event, relayAuthor)` accepts only relay-signed recognized payloads `member_joined|member_left|member_removed` (`src/features/relay/membership.ts:12+`). The row has empty content and no actions.
- **Overlay indexing** (`:257-273`): every non-row event is listed under each of its `e` tag values. The **39005** case is special: it is used only if `pubkey == relayAuthor`, it is keyed by its **first** `e` tag, and `newer()` wins (higher `created_at`, tie goes to the lexicographically **smaller** id; `src/features/relay/events.ts:141-149`).
- **Displayed author** `messageAuthor` (`:202-219`): if `signingAuthority` (NIP-11 `self`) signed the event, use the `["actor",<hex>]` tag. If absent, use the first tag when it is `["p",<hex>]` and an `h` tag exists. Otherwise use `event.pubkey`. When it differs, `signerId = event.pubkey`.
- **Deletes** `deletionApplies(del, target)` (`:225-243`). A target is deleted if **any** overlay deletion satisfies one of:
  - (a) `del.kind ∈ {5, 9005}` and `del.pubkey == target.pubkey`, or
  - (b) `del.kind ∈ {5, 9005}`, target kind ∈ {9, 40002}, and `del.pubkey == messageAuthor(target)`, or
  - (c) `del.kind == 5`, target kind ∈ {9, 40002}, and the deletion has `["k", "<target.kind>"]`. **No author check here.** The client trusts relay ingest, which requires author or agent owner (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:366-424`).
  - Deleted rows are dropped entirely. There is no tombstone row.
- **Edits** (kind 40003; tags `["h",ch],["e",target], imeta*, emoji*`) (`:318-328`):
  - The edit counts only if `edit.pubkey ∈ {target.pubkey, messageAuthor(target)}` and the edit itself is not deleted.
  - Order: `created_at DESC`, then id ASC. The first edit is current. Its content replaces the body. For 40002, the body is JSON-parsed and `.content` is used.
  - `edited:true` when any valid edit exists.
  - Attachments come from the newest edit **with** an `imeta` tag, else the original (`attachmentSourceId` is set when it is an edit).
  - Emoji mappings come from the newest edit if it has `emoji` tags, else the original.
  - Mentions (`p`) always come from the **original**.
  - The relay also checks edit ownership at ingest (`buzz:crates/buzz-relay/src/handlers/ingest.rs:2924-2928`).
- **Reactions** (kind 7; tags `["h",ch],["e",target],["emoji",sc,url]?`) `groupReactions` (`:433-487`):
  - Group key = `(content, emoji url)`. Custom emoji applies when `content == ":shortcode:"` and the reaction carries a matching `emoji` tag.
  - Each group holds `{id, authorId=pubkey}` entries and drops deleted ones. A reaction can be deleted only by its raw pubkey (rules a/b/c do not extend to non-message kinds).
  - Empty groups are dropped. Groups are ordered by first reaction `(created_at, id)`. Counting is per event; the UI counts people.
- **Thread summary** `parseSummary` (`:167-197`): `replyCount = descendant_count` (a non-negative integer), else `reply_count`, else 0. `participants` = 64-hex strings, deduped.
- **Other fields**:
  - `threadRootId`/`replyParentId` come from `threadReference` (§5c.1).
  - `sentFromThread` comes from `["buzz:sent-from-thread", rootId, excerpt≤64 chars]`.
  - `mentionReferences` come from 2-element `["mention",hex]` tags.
  - `diff` metadata (40008) comes from the `file`, `repo`, `commit`, `description`, and `truncated` tags.
  - Attachments come from `imeta` plus Markdown images (`:83-165`).

**Admin/moderator deletes.** The relay accepts NIP-29 `9005` from channel admins and moderators and **soft-deletes** the target server-side, decrementing thread counters (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1806-1880`). Later window reads omit it. The client fold does **not** hide a row because of a *live* moderator 9005 unless the signer equals the author (rule a/b). The row disappears only when a head reload replaces `state.events`. `catchUp` merges, so a row from an older page can persist. This is a likely bug or gap to decide on in the port.

**Server-side delete constraint:** the relay rejects kind 5/9005 unless it has **exactly one** `e`/`a` target (`buzz:crates/buzz-relay/src/handlers/ingest.rs:2908-2921`). The client's `messages.remove()` emits up to 100 `e` tags plus `k` tags (`src/features/relay/messages.ts:262-311`). Multi-select removal would be rejected by this relay.

### 5b.6 Window limits and eviction

| Limit | Value | Source |
|---|---|---|
| Open history windows (LRU) | 3 (`maxWindows`). With `prepared`, mounted readers are never evicted. | `store.ts:137,356-364` |
| Rows / bytes per window | 2400 rows / 8 MiB, then `historyLimited` | `store.ts:143-144,638-648,2206-2235` |
| Prepared heads | 64 / 4 MiB `ByteLru` | `store.ts:141-142,179` |
| Head freshness lease | 60 000 ms | `store.ts:99` |
| Live tails | 256 events/channel, 64 channels / 4 MiB | `store.ts:181-184,2155` |
| Idle snapshot cache | 256 | `store.ts:334` |
| Disk heads | 64 records / 8 MiB, 24 h | `store.ts:717`; `docs/channels.md:989-993` |
| Shared verified observations | 4096 events / 8 MiB | `docs/relay-queries.md:410` |

Eviction (`evict`, `store.ts:348-355`): bump the generation, abort the controller, drop events, and reset to an idle snapshot. Access revocation (`purgeAccess`, `:2256-2335`) re-filters heads, tails, and windows through `visible()`, aborts all reads, and wipes disk heads.

---

## 5c. Threads

### 5c.1 NIP-10 tags

**Client parse** `threadReference(event)` (`src/features/relay/thread-reference.ts:1-13`), which matches the relay's `buzz:crates/buzz-core/src/nip10.rs:27-80`:
- Only `["e", <64-hex>, <relay>, <marker>]` tags count. Bare or positional `e` tags are ignored, as are malformed IDs.
- **The last valid occurrence** of each marker wins. IDs are lowercased.
- `reply` present: `{rootId: root ?? reply, parentId: reply}`.
- `root` only, or neither: `undefined`, meaning top-level. A lone `root` never makes a reply.

**Emitted shapes** (`src/features/relay/messages.ts:179-207`):
```jsonc
// direct reply to root R
{"kind":9,"content":"…","tags":[["h","<ch>"],["e","R","","reply"], ["p",pk]…, ["mention",pk]…, ["emoji",sc,url]…, imeta…]}
// nested reply to P under root R
{"kind":9,"content":"…","tags":[["h","<ch>"],["e","R","","root"],["e","P","","reply"], …]}
```
- The relay hint (position 2) is always `""`. The marker is at index 3.
- `p` tags hold mention recipients only, deduped, ≤32, validated against channel membership (`messages.ts:56-65`). The client does **not** auto-add the parent author's `p`.
- `["broadcast","1"]` (a depth-1 reply also shown on the channel timeline) is understood by readers (`fold.ts:291-292`, `unread.ts:436`). **No buzz-app sender emits it.**
- Relay ingest:
  - Depth = parent depth + 1, max **100** (`buzz:crates/buzz-relay/src/handlers/ingest.rs:987,1198`).
  - The parent must be in the same channel, and the `root` marker must agree with the ancestry (`buzz:docs/nips/NIP-CW.md:94`).
- "Send to channel too" is implemented differently: it posts a separate top-level kind 9 with `["buzz:sent-from-thread", rootId, excerpt?]` (`messages.ts:130-178`).

### 5c.2 Thread summaries in the channel timeline

- Source:
  - (a) A window page with `include_summaries:true` gets one 39005 per row with ≥1 reply.
  - (b) **Live**: after each reply or delete commit, the relay re-reads `thread_metadata` and publishes a fresh 39005 on the channel topic (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:887-971`).
- Shape (`buzz:crates/buzz-relay/src/api/bridge.rs:816-839`; `buzz:docs/nips/NIP-CW.md:127-147`):
  ```json
  {"kind":39005,"pubkey":"<relay>","tags":[["e","<root>"],["d","<root>"],["h","<chan>"]],
   "content":"{\"reply_count\":4,\"descendant_count\":7,\"last_reply_at\":1751500123,\"participants\":[\"<hex>\",…≤10, most recent first]}"}
  ```
- Client: `ChannelMessage.replyCount = descendant_count || reply_count`, and `participants` (`fold.ts:167-197`). The latest summary wins by `created_at`. The tie-break is the smaller id, which can pick a stale count when two summaries share a second (uncertain risk).
- The UI shows the reply count plus up to 3 participant avatars and `+N`. Clicking opens the thread panel (`docs/channels.md:1086-1092`).
- Replies themselves never appear as channel rows (unless broadcast). They are fetched only by the thread view.

### 5c.3 Thread view lifecycle (`createThreadView`)

`src/features/relay/threads.ts:59-623`; allocated by `session.thread(channelId, messageId, {exact?})` (`src/features/relay/session.ts:1780-1859`). `messageId` must be 64 hex. Thread views share a cap of **64** owned views with filtered views.

Constants (`threads.ts:18-25`):

| Name | Value |
|---|---|
| `AUX` | {5, 7, 9005, 40003, 39005, 39006} |
| `INITIAL_WINDOW_SIZE` | 10 |
| `PAGE_SIZE` | 50 |
| `MAX_PAGES` | 10 |
| `MAX_EVENTS` | 2000 |
| `MAX_BYTES` | 4 MiB |
| `STRICT_WINDOW_KINDS` | [9, 40002] (relay allowlist excludes 40008) |
| Content kinds | 9, 40002, 40008 |

Snapshot (`:34-49`): `{status: idle|loading|ready|error, root, replies, error, canLoadMore, limited, direction?: "older"|"forward", readKind?: "older"|"refresh", target?, targetStatus?}`.

Mode (`:103-105`): `"probe"` if `channelId` is a canonical lowercase UUID (`/^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/`, `thread-window.ts:14`). Otherwise `"legacy"`.

**Seed** (`:272-276`): if the message is in the session's recent cache or a retained window, `rootId = threadReference(seed)?.rootId ?? seed.id`.

**`run(replace)`** (`:300-596`), where `refresh()` = `run(true)` and `loadMore()` = `run(false)`:
1. If no `rootId`, read `[{ids:[messageId], "#h":[ch], limit:1}]`. The selected event must be a content kind in the channel. Then `rootId = threadReference(event)?.rootId ?? event.id`.
2. **Root read** (strict modes, once per run): `[{ids:[rootId], "#h":[ch], limit:1}]`. The root must be a content kind in the channel with **no** reply marker. If not, set `rootUnavailable`, throw "The original thread message is unavailable.", and keep known tombstones.
3. **Strict page** (probe/older):
   ```json
   [{"kinds":[9,40002],"#h":["<ch>"],"#e":["<root>"],"depth_limit":100,"limit":10|50,
     "include_aux":true,"thread_window":true, "until":<c.created_at>,"before_id":"<c.id>"}]
   ```
   `limit` is 10 for the first page of a run and 50 after. `until`/`before_id` appear only when continuing. The page must contain one 39007, already verified by the reader. `bounds = threadBounds(39007)`, `mode = "older"`, and `nextCursor = bounds.cursor`.
4. **Legacy fallback**: only when the **first probe** throws `MissingThreadBounds` with `legacyRows=true`, meaning a 200 that contained verified rows for this `#h`/`#e` but no 39007. The client then restarts with clean state:
   ```json
   [{"ids":["<root>"],"#h":["<ch>"],"limit":1},
    {"kinds":[9,40002,40008],"#h":["<ch>"],"#e":["<root>"],"depth_limit":100,"limit":50,"include_aux":true,
     "thread_cursor":<last reply created_at>,"thread_cursor_id":"<last reply id>"}]
   ```
   - The legacy cursor is the **last loaded reply** in `(created_at ASC, id ASC)` order.
   - "Not advancing" fails with "Thread pagination did not advance".
   - `more = replies.length > 0`.
   - An empty unsigned probe stays an error. It is not treated as a fallback, because it could be access denial.
   - Invalid bounds, errors, or missing bounds after a strict success never fall back.
   - Non-UUID channels go directly to legacy mode.
5. Pages loop: `targetPages = replace ? max(1, pages) : 1`. Refresh re-reads the whole retained range from the head. Strict mode stops when `!bounds.hasMore`.
6. Retention (`retain`, `:241-263`): after each run, `remote = union(remote, fetched)`. **Omitted events are not retractions.** If the union exceeds 2000 events or 4 MiB, everything is cleared, with `limited:true` and the error "Thread view exceeded its memory limit. Refresh to read from the beginning." Nothing is silently evicted.
7. Final status: `canLoadMore = more && pages < 10` and `limited = more && pages >= 10`. Strict mode therefore holds at most 10 + 9×50 = 460 replies. Legacy mode holds 10×50.

**`related(events)`** (`:119-150`) selects which events belong to the thread:
- Rows: non-AUX, non-39007 events with `h == ch`, where `id == rootId`, or `id == messageId` (exact mode), or `threadReference(e).rootId == rootId`.
- AUX: 2 hops of events whose `e` targets a known ID. This covers delete-of-reaction.

**Fold** (`publish`, `:173-240`):
- Input: `foldMessages(ch, relay, remote ∪ related(local outbox ops), {includeReplies:true, signingAuthority})`. Failed non-content local ops are excluded.
- `root` = the row with `id == rootId`.
- `replies` = the other rows sorted by `compareMessages`. Each reply raises `MessageClock`.

**Live:** `session` calls `thread.receive(events)` for every verified observation (`session.ts:1847-1848`). `receive` runs `related()`, unions the result into `remote`, and republishes. The thread has no separate subscription and no cursor change. Local replies appear immediately in the thread but not in the channel timeline (the fold excludes `reply`).

**Reply tree (UI)** `replyTree(rows, rootId)` (`src/features/messages/reply-tree.ts:4-40`):
- Parent = `replyParentId` if that row is loaded and is not the root. Otherwise the reply attaches at top level under the root.
- Cycles are broken by attaching at top level.
- Siblings follow the input order (`compareMessages`).
- UI: direct replies are flat. Nested branches start collapsed and expand one level at a time (`docs/channels.md:1121-1128`).

**Composer:** reply to the root via `messages.reply(ch, rootId, text, mentions, attachments, parentId=rootId)` (§5c.1). Drafts are keyed `draft:<ch>:thread:<rootId>` (`docs/channels.md:1211-1212`).

### 5c.4 Thread windows: kinds 39005 / 39006 / 39007

| Kind | Name | Signed by | Scope | Stored? |
|---|---|---|---|---|
| 39005 | thread summary | relay identity (NIP-11 `self`) | one per root; `e`=`d`=root, `h`=channel | no; synthesized per window page, plus a live publish after each thread change |
| 39006 | channel window bounds | relay | one per served channel-window response; `d=<ch>:head\|<ch>:<ts>:<id>`, `h` | no |
| 39007 | thread window bounds | relay | one per served `thread_window` filter; tags exactly `d`,`h`,`e` | no |

All three are relay-only kinds. Client-submitted copies are rejected at ingest (`buzz:crates/buzz-core/src/kind.rs:435-441,850-852`; `buzz:docs/nips/NIP-CW.md:125,360`).

**39007** (`buzz:crates/buzz-relay/src/api/bridge/thread_window.rs:216-237`; `buzz:docs/nips/NIP-CW.md:306-336`):
```json
{"kind":39007,"pubkey":"<relay>",
 "tags":[["d","tw:1:<sha256 hex>"],["h","<channel uuid>"],["e","<root id>"]],
 "content":"{\"version\":1,\"direction\":\"older\",\"has_more\":true,\"next_cursor\":{\"created_at\":1751500000,\"id\":\"<hex>\"}}"}
```
Binding = SHA-256 of the compact JSON array (`buzz:crates/buzz-core/src/thread_window.rs:162-182`; client `src/features/relay/thread-window.ts:32-61`):
```
["tw",1,"older",<host>,<viewer hex>,<channel>,<root>,<limit>,<depth_limit|100>,<sorted distinct kinds>,<null | [until,"before_id"]>,<include_aux|false>]
```
- `host` = `threadAuthority(transport.scope)`: URL host, lowercased, with `:80`/`:443` stripped and any trailing `.` removed. Userinfo or a non-http(s)/ws(s) scheme is rejected (`thread-window.ts:17-29`). It must be the **relay** authority, never a local broker host.
- The client JSON must serialize exactly like serde's compact output, with no spaces.

**Server thread_window request rules** (`buzz:crates/buzz-core/src/thread_window.rs:83-156`; `buzz:crates/buzz-relay/src/api/bridge/thread_window.rs`):
- Allowed keys are **only** `thread_window, #h, #e, kinds, limit, depth_limit, until, before_id, include_aux`. Anything else returns 400.
- `#h`: exactly one UUID. `#e`: exactly one 64-hex.
- `kinds`: 1–4 values from {9, 40002, 45001, 45003}, sorted and deduped.
- `limit`: 1..200, default 50. `depth_limit`: 1..100, default 100.
- The cursor needs both fields or neither. `until ≥ 0`.
- At most 4 thread windows per query, and they cannot mix with other modes.
- Processing:
  - The root must be a supported kind in that channel. Otherwise the result is no events and **no bounds**.
  - Replies at depth 1..depth_limit, `created_at DESC, id ASC`, probing `limit+1`.
  - Aux closure over root + rows.
  - One 39007 per served filter.
- Whole-batch limits: an 8 s deadline (`:18`); 8 MiB output (`:19`); 64 aux scans / 8192 raw rows / 8 MiB content (`buzz:docs/nips/NIP-CW.md:287-304`).
- The batch's access set is re-checked at the end. Any change gives 503 "thread authorization changed; retry query" (`:90-119`). Budget or timeout gives 503. Corruption gives 500.

**Client verification** `verifyThreadWindows(filters, events, scope, viewer, relayAuthor)` (`thread-window.ts:93-158`). It runs inside the reader before the session sees any event (`reader.ts:165-171`):
- If any filter has `thread_window:true`, all must. `scope` must exist (missing scope is a config error and must **not** fall back), and viewer/relay must be hex.
- No 39007 at all: throw `MissingThreadBounds(legacyRows)`.
- Otherwise:
  - The count of 39007s must equal the count of filters.
  - Each filter needs exactly one 39007 whose `d == binding`, `pubkey == relayAuthor`, `tags.length == 3`, with exactly one each of `["d",binding]`, `["h",ch]`, `["e",root]` (2-element tags).
  - The content must have `version == 1`, `direction == "older"`, a boolean `has_more`, and `next_cursor` null or `{created_at: safe int in 0..8 210 266 876 799, id: 64 hex}`, with `has_more ⇔ next_cursor != null` (`:63-89`).
  - When continuing, `next_cursor` must be strictly older than the request cursor (`:148-156`).

**Legacy thread path** (relay, `buzz:crates/buzz-relay/src/api/bridge.rs:1572-1673`; `buzz:docs/nips/NIP-CW.md:178-220`):
- Selected by `depth_limit` + one `#e` with no `thread_window`. `#h` is optional, and if present it must be accessible.
- `limit` defaults to 100 and is capped at 500. Order is `created_at ASC, id ASC`. `thread_cursor` gives `created_at > c OR (= c AND id > cid)`. `thread_cursor: -1` is accepted as a start sentinel. `depth_limit 2147483647` means unbounded.
- **The root itself is not returned.** The client batches a separate `{ids:[root]}` filter for it.
- `include_aux` covers root and replies.
- `kinds` does not restrict the rows on this path.
- There are no bounds, so a short page is only a heuristic for the end.

### 5c.5 Opening from a deep link / exact message

Link formats: `buzz://message?channel=<id>&id=<event>[&thread=<root>]`, `buzz://channel/<id>/<event>` (`docs/deep-links.md:17-19`). Parsing is in `src/features/navigation/buzz-links.ts:73-102`. A `thread` param classifies the link as "thread". It is a **hint only**: the root is always re-derived from signed ancestry (`docs/relay-queries.md:256-262`).

`ThreadPanel` uses `exact:true` when the navigation target's `threadRootId != messageId`, or when required by reveal/ready-root (`src/features/messages/ThreadPanel.tsx:176-186`). Exact mode (`threads.ts:328-395`) adds up to 3 reads before the normal traversal:
1. `[{ids:[messageId], "#h":[ch], limit:1}]`: the event must be a content kind in `ch`. Admit it, then set `rootId = threadReference(ev)?.rootId ?? ev.id`.
2. `[{kinds:[5,7,9005,40003,39005], "#e":[messageId], limit:500}]`: the overlays.
3. If any overlays target `messageId`: `[{kinds:[5,9005], "#e":<overlay ids>, limit:500}]` (tombstones of overlays).
- The exact reader wrapper rejects any raw response of **≥500 events** (a non-`depth_limit` read) with "Selected message exceeded its evidence limit" (`session.ts:1822-1829`). Overlays are admitted atomically only after the fold completes (`admit`).
- `targetStatus`: `loading` → `ready` | `unavailable` (missing or deleted) | `error`.
- An accessible reply stays visible even if its root is missing or outside the traversal cap. Normal bounded traversal then supplies surrounding context, and the selected ID never becomes a cursor.
- A top-level target that is already in the loaded channel window is revealed in the timeline. An off-window target opens as the thread root in the panel. There is **no "around message" channel query** (`docs/channels.md:1322-1337`).

### 5c.6 Thread follows

`src/features/relay/thread-follows.ts:1-68`; semantics in `src/features/relay/unread.ts:101-106,470-500,1850-1876`:
- Device-local `localStorage` key `buzz.thread-follows.v1:<scope>` (scope = community+viewer). The value is a JSON array of `[ "<channelId>:<rootId lowercase>", boolean ]`. `true` means explicitly followed, and `false` means explicitly unfollowed (this overrides participation).
- Cap: `THREAD_FOLLOW_LIMIT = 1000`. Newest entries win, and the oldest are deleted on insert. Cross-window sync uses the `storage` event. `forgetThreadFollows(scope)` runs on leaving a community.
- When no explicit choice exists, the viewer follows a thread automatically after writing or replying anywhere in it (`ownThreads`). This drives unread and notification counting of replies. It is not synced to the relay.

### 5c.7 Not found / explicitly unknown

- No client sender emits `["broadcast","1"]`, `feed_types`, or `resolve_thread_roots`.
- The NIP-98 `nonce` tag is a client addition. I did not check whether the relay requires it.
- `enforce_relay_membership` (`buzz:crates/buzz-relay/src/api/bridge.rs:1325-1332`) uses an `X-Auth-Tag`-style header (`extract_auth_tag_header`). I did not trace its exact header name or status codes.
- The exact relay default `limit` for a catch-all filter with no `limit` is untraced. The client always sends one.

---

## 5d. Sending a message, edits, deletes, reactions

### 5d.1 Kinds involved

| Kind | Meaning | App writes? | App reads? | Source |
|---|---|---|---|---|
| 9 | Channel message (NIP-29 chat). **This is the kind the app sends.** | yes | yes | `src/features/relay/messages.ts:85-98`; `buzz:crates/buzz-core/src/kind.rs:481` |
| 40002 | Agent "envelope" message; content is JSON `{"content": "..."}` | no | yes (unwraps `.content`) | `src/features/relay/fold.ts:323-326`; `buzz:crates/buzz-core/src/kind.rs:483` |
| 40008 | Diff message (tags `file`, `repo`, `commit`, `description`, `truncated`) | no | yes | `fold.ts:370-385` |
| 40003 | Edit | yes | yes | `messages.ts:209-244` |
| 5 | NIP-09 deletion (own message / own reaction / agent-owner) | yes | yes | `messages.ts:265-314` |
| 9005 | NIP-29 delete-event (moderator / native) | **no** | yes (treated like 5) | `fold.ts:225-243`; SDK `buzz:crates/buzz-sdk/src/builders.rs:432-461` |
| 7 | Reaction | yes | yes | `messages.ts:245-264` |
| 1984 | NIP-56 report | yes (direct publish, bypasses outbox) | no | `messages.ts:316-335` |
| 39005 | Relay-signed thread summary (reply_count / descendant_count / participants) | no | yes | `fold.ts:261-265`, `fold.ts:165-195` |

Live channel subscription kinds (`#h` filter): `[9, 40002, 40008, 45001, 45003, 40099, 40100, 40003, 5, 9005, 7, 39000, 39002, 39005, 20002]` (`src/features/relay/live.ts:200-203`).

### 5d.2 Top-level message (kind 9)

Builder: `createMessages().send(channelId, content, mentions, attachments, recovery?, references)` (`messages.ts:75-99`). Tag order is fixed:

1. `["h", channelId]`
2. one `["p", pubkey]` per **recipient** (notified mention) — deduped, max 32, each must be a current channel member (`messages.ts:56-65`; member check `src/features/relay/session.ts:951-968`)
3. one `["mention", pubkey]` per **reference** (non-notifying, 2 elements) — deduped, max 32 (`messages.ts:66-73`)
4. `["emoji", shortcode, url]` for each community custom emoji shortcode found in the *text the user typed* (`messages.ts:93`, resolver `src/features/relay/emoji-directory.ts:292-311`)
5. `["imeta", ...]` per uploaded attachment (`messages.ts:94`; built in `src/features/relay/attachments.ts:284-320`, see media section)

Content: `content.trim()`; empty after trim -> error "Message is empty" (`messages.ts:51-55`). When attachments exist, the content becomes `[text, ...attachmentMarkdown].join("\n\n")` where each attachment is `![label](<url>)` for image/png|jpeg|gif|webp and video/mp4, else `[label](<url>)` (`attachments.ts:98-106`, `attachments.ts:316-319`).

No `client` tag, no `broadcast` tag, no `alt`, no NIP-10 `e` tag on top-level messages are written by the app (grep confirms; `broadcast` is only read: `fold.ts:290-293`, `projection.ts:29`).

```json
{
  "kind": 9,
  "content": "@Alice can you check this? :partyparrot:\n\n![screenshot.png](<https://relay.example/media/ab12...ef.png>)",
  "tags": [
    ["h", "3f0c2a8e-6b1d-4c1e-9a55-0d4f6c2b9e11"],
    ["p", "a1b2...<64hex alice>"],
    ["mention", "c3d4...<64hex bob, non-member reference>"],
    ["emoji", "partyparrot", "https://relay.example/media/77aa...png"],
    ["imeta", "url https://relay.example/media/ab12...ef.png", "m image/png", "size 48213", "x ab12...ef", "filename screenshot.png"]
  ]
}
```

Limits:
- Composer `maxLength={16000}` chars (`src/features/messages/MessageComposer.tsx:1507`; `composer-document.ts:545`).
- SDK `build_message` caps content at 64 KiB and mentions at `MENTION_CAP = 50` (`buzz:crates/buzz-sdk/src/builders.rs:240-268`, `buzz:crates/buzz-sdk/src/mentions.rs:38`). The app's stricter cap is **32 p + 32 mention**.
- Renderer bounds: `MAX_MARKDOWN_LENGTH = 100_000`, depth 100 (`src/features/relay/message-content.ts:3-4`).
- Writer gating: channel must be participable ("Join the conversation before posting") and the outbox must support the kind (`messages.ts:42-48`).

### 5d.3 Mention encoding (content vs tags)

Spec: `src/bundled/mentions/README.md` ("Mention rules v2", section 8 "Tags (normative)"), fixtures `src/bundled/mentions/mention-rules.fixtures.json`.

- **Content** carries plain text `@<display name>` (e.g. `@Alice`). It is not `nostr:npub`, and there is no placeholder or index. The draft model keeps `{pubkey, name, start, end}` spans where `text.slice(start,end) === "@"+name` and the next char is a boundary `[\s.,!?;:()[\]{}*~\`]` or end of text (`src/features/messages/mention-draft.ts:16-56`). Only explicitly chosen chips become recipients. Prose is never parsed to infer recipients (`mention-draft.ts:57`).
- The composer serializes emphasis around a mention with `*` and not `_`, so `*@Honey*` still binds (`src/features/messages/composer-markdown.ts:287-296`).
- **Tags**: recipient -> `["p", key]` (notifies/wakes). Reference -> `["mention", key]` (display only, never notifies). Readers MUST ignore the 3-element form `["mention", key, "agent-address"]` that other Buzz clients write next to a `p` tag (README section 8). A key in both lists counts as addressed.
- **Member vs outside**: a recipient must be a channel member when the message is written. For outside keys in stream/forum channels the client asks first: *Invite* adds them with kind 9000, waits for confirmation, then sends them as `p`. *Do nothing* / *Send anyway* sends them as `mention` references. In DMs, outside keys are always sent as references, with no prompt (README section 7).
- **Display binding (read side)**: `profileMentionParts` binds `@Name` to a key only if the name is the kind-0 name (or agent library name) of a key in the message's `p` or `mention` tags. Longest name wins. An ambiguous name stays plain. The `@` must not follow `[\p{L}\p{N}_@/\\]`, and the char after the name must not be `[\p{L}\p{N}_]`. Code fences, indented code, inline code, markdown links, and URLs including `nostr:` are skipped. Edited messages (any 40003) are **never** bound (`src/features/messages/profile-mentions.ts:8-85`, `:89-128`). A legacy qualifier ` (<64hex>)` after a name is skipped (`profile-mentions.ts:69-75`).
- `nostr:npub1…` in content is a profile *link* target only (docs/profiles.md "Boundaries"). It is not a notification. The SDK can auto-resolve `@name`/`nostr:npub` into p-tags (`buzz:crates/buzz-sdk/src/mentions.rs:1-30`), but the app does **not** do this.
- Mention query syntax: `@` at start or after whitespace/`(`/`[`/`{`. No `@`, newline or tab in the query. Look back at most 160 chars (README section 2). Matching uses NFKD with diacritics stripped and lowercase (README section 3).

### 5d.4 Thread replies (reference only)

`reply(channelId, rootId, content, mentions, attachments, parentId=rootId, references)` (`messages.ts:179-207`). Tags: `["h", ch]`, then `["e", rootId, "", "root"]` **only if parentId ≠ rootId**, then `["e", parentId, "", "reply"]`, then p, mention, emoji and imeta as above. This matches SDK `thread_tags` (`buzz:crates/buzz-sdk/src/builders.rs:177-191`).

```json
{"kind":9,"content":"agreed","tags":[["h","<uuid>"],["e","<root>","","root"],["e","<parent>","","reply"]]}
```
A direct reply to the root has only `["e", root, "", "reply"]`.

Top-level timeline excludes events that have an `e …"reply"` tag, unless they have `["broadcast","1"]` (`fold.ts:288-294`). SDK writes `["broadcast","1"]` when `broadcast=true` (`buzz:crates/buzz-sdk/src/builders.rs:256-258`). **The app never writes `broadcast`.** It uses a different "send to channel" mechanism (next).

Media-time replies prefix content as `⏱ <m:ss> — <text>` (`src/features/messages/media-timecode.ts:19-21`).

### 5d.5 "Also send to channel" (sent-from-thread copy)

`sendToChannel(row, root)` publishes a **new top-level kind 9** copy of the user's own thread reply (`messages.ts:100-178`):

```json
{
  "kind": 9,
  "content": "<reply source content, or attachment URLs joined by \\n if empty>",
  "tags": [
    ["h", "<uuid>"],
    ["buzz:sent-from-thread", "<rootId>", "<root excerpt ≤64 code points, optional>"],
    ["mention", "<key>"],
    ["emoji", "<code>", "<url>"],
    ["imeta", "..."],
    ["link-preview", "..."]
  ]
}
```
- Original recipients are downgraded to `mention` references, so there is no re-notification. They are omitted entirely if the reply was edited (`messages.ts:162-167`).
- Excerpt: the root content with spoilers `||…||`, images, link URLs, bare URLs and `` `*_~>#| `` stripped and whitespace collapsed. If longer than 64 code points it is cut to 63 plus `…` (`messages.ts:134-150`).
- Reader: `sentFromThread = {rootId, excerpt}` (`fold.ts:354-367`).
- Only allowed on the user's own kind-9 reply with delivery `accepted`/`seen`.

### 5d.6 Workflow attribution (read-only)

Workflow output is a **relay-signed** kind 9 (`buzz:crates/buzz-relay/src/workflow_sink.rs:291-312`):
```json
{"pubkey":"<relay NIP-11 self>","kind":9,"tags":[["p","<owner>"],["h","<uuid>"],["buzz:workflow","true"],["buzz:workflow-owner","<owner>"],["p","<mentioned>"],["buzz:workflow-mention","<mentioned>"]]}
```
- Displayed author (`messageAuthor`, `fold.ts:202-219`): if `event.pubkey == signingAuthority` (the relay's NIP-11 `self`), the author is the first `["actor", hex]` tag. Otherwise it is the leading `tags[0]` `p` when an `h` exists. Otherwise it is the signer.
- `workflowOwner` (`src/features/relay/workflow-attribution.ts:5-26`) applies only when signer == authority, kind 9, exactly one `["buzz:workflow","true"]` (len 2) and exactly one `["buzz:workflow-owner", hex64]` (len 2), and the event has admission-time signature proof. The label is `Workflow · owned by <name>` (`workflow-attribution.ts:29-31`).

### 5d.7 Edit (kind 40003)

`edit(messageId, content, attachmentSourceId)` (`messages.ts:209-244`):
```json
{
  "kind": 40003,
  "content": "fixed typo :partyparrot:",
  "tags": [
    ["h", "<uuid of original>"],
    ["e", "<original message id>"],
    ["imeta", "..."],
    ["emoji", "partyparrot", "https://..."]
  ]
}
```
- Target must be kind 9 or 40002 and owned by the viewer (`messageAuthor(original) === viewer`).
- `imeta` tags are copied from the current attachment source: the original event, or the latest prior 40003 that carried `imeta`. Edits cannot add or remove attachments. New `p`/`mention` tags are **not** written, so recipients cannot change. SDK `build_edit` writes only `h` + `e` (`buzz:crates/buzz-sdk/src/builders.rs:407-418`).
- Relay authorization (`buzz:crates/buzz-relay/src/handlers/ingest.rs:1256-1334`): the target must exist in the same channel. The editor must be the effective author (and still a member, or the channel is `open`) **or** the NIP-OA owner of the agent author. Otherwise it rejects with "must be event author to edit". Generic membership gate is skipped for 40003 (`buzz:crates/buzz-relay/src/handlers/ingest.rs:2693-2702`).
- Fold (`fold.ts:318-327`, `:416-421`): edits whose author is the message signer or effective author and are not themselves deleted. Latest wins: `created_at` desc, then `id` asc. `emoji` tags come from the latest edit if it has any, otherwise from the original. Row gets `edited: true`. Mentions are still from the original.

### 5d.8 Delete (kind 5; kind 9005 read-only)

`remove(eventIds, authorization?)` (`messages.ts:265-314`):
```json
{
  "kind": 5,
  "content": "",
  "tags": [
    ["h", "<uuid>"],
    ["e", "<target id>"],
    ["k", "9"]
  ]
}
```
- 1–100 ids, all in one channel. Targets kind 7, 9 or 40002. One `k` tag per distinct target kind (string). For a reaction, the channel is taken from the reaction's `h`, or from its target's `h`.
- Own reactions: `event.pubkey === viewer`. Own messages: `messageAuthor === viewer`. **Agent-owner removal**: messages authored by an agent whose verified NIP-OA owner is the viewer (`AgentOwnerAuthorization {agentId, ownerId}`).
- **Relay constraint (important):** `buzz:crates/buzz-relay/src/handlers/ingest.rs:2908-2921` rejects any kind 5 or 9005 unless it has **exactly one** `e` or `a` target ("deletion events must reference exactly one target"). The app's multi-id form (e.g. removing duplicate own reactions in one group, `src/features/messages/MessageReactions.tsx:82`) will be rejected by this relay build. **Native port: emit one kind 5 per target.** Relay authorization: actor == effective author of the target, or the NIP-OA owner of it (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:366-423`).
- Kind 9005 (moderator/admin, SDK only) shape: `["h",ch]`, `["e",id]`, optional `["action_id",uuid]`, `["reason_code",s]`, `["public_reason",s]`, empty content (`buzz:crates/buzz-sdk/src/builders.rs:440-461`). The app never writes it.
- Read rule `deletionApplies` (`fold.ts:225-243`): a 5/9005 removes its target if its signer == target signer, or (for message kinds 9/40002) signer == effective author, or (kind 5, message kinds) it carries `["k", String(target.kind)]`. The last case trusts the relay's owner check. Deleted 40003 edits are ignored.

### 5d.9 Reactions (kind 7) and removing them

`react(messageId, content, emoji?)` (`messages.ts:245-264`). Target must be kind 9, 40002 or 40008.
```json
{"kind":7,"content":"👍","tags":[["h","<uuid>"],["e","<message id>"]]}
```
```json
{"kind":7,"content":":partyparrot:","tags":[["h","<uuid>"],["e","<message id>"],["emoji","partyparrot","https://relay.example/media/77aa...png"]]}
```
- **No `p` tag and no `k` tag** are written. This differs from NIP-25's recommendation. SDK `build_reaction` also writes only `e` (`buzz:crates/buzz-sdk/src/builders.rs:492-522`). The `h` tag is not required by the relay for kind 7 (`buzz:crates/buzz-relay/src/handlers/ingest.rs:769-798`), but the app adds it so `#h` subscriptions receive the reaction.
- Content: trimmed, non-empty, and either ≤64 code points or `:[a-z0-9_-]{1,64}:` (case-insensitive) (`src/features/relay/emoji.ts:6-10`). The app never sends `"+"`. Quick-reaction defaults are `["👍","❤️","😂"]` plus the top 24 per scope in localStorage `buzz.quick-reactions.v1:<scope>`, showing 3 (`src/features/messages/quick-reactions.ts:5-37`).
- Custom emoji tag: if `emoji` is passed and `content.toLowerCase() === ":"+shortcode+":"`, write exactly `["emoji", shortcode, url]`. Otherwise fall back to catalog lookup (`messages.ts:259-261`).
- **Remove reaction** = kind 5 as in 5d.8 with `["k","7"]` and the reaction id(s). Toggling an existing group removes all of the viewer's events in that group (`MessageReactions.tsx:82`). Send one kind 5 per id (see the relay constraint above).
- Grouping (`fold.ts:433-487`): key = `[content-or-:code:, emoji url]`. A custom match requires `content.toLowerCase() == ":"+tag shortcode+":"` with a matching `emoji` tag. Groups are ordered by first `created_at` then id, keep `{id, authorId}` per event, and drop deleted events.

### 5d.10 Report (kind 1984)

```json
{"kind":1984,"content":"<note>","tags":[["p","<message signer pubkey>"],["e","<message id>","spam"]]}
```
Types: `spam|profanity|nudity|impersonation|malware|illegal|other`. Signed and published directly with a 10 s timeout, not through the outbox (`messages.ts:17-25`, `messages.ts:316-335`, `session.ts:1752-1772`).

### 5d.11 Misc

- `body.ts` provides only `objectBody(content)`: JSON.parse to a plain object or undefined. It is used for 40002 and 39005 (`src/features/relay/body.ts:1-14`).
- `receipt.ts` provides `readReceiptText` (`src/features/relay/receipt.ts:1-21`): it reads a command-receipt HTTP body, max **16 KiB**, strict UTF-8.
- `message-link-parts.ts` handles display-only link parsing: `[label](url)`, `<https://…>`, bare `http(s)://` and `buzz://` URLs, with trailing punctuation and balanced-paren trimming. Valid schemes are http/https without credentials, plus `buzz:` deep links (`src/features/messages/message-link-parts.ts:5-16`, `:89-147`).

---

## 5e. Direct messages

### 5e.1 Open DM (kind 41010)

Client event (`src-tauri/src/relay/channel_writes.rs:275-335`), sent by a purpose-bound native
command; generic `POST /events` of 41010 is refused client-side (`src-tauri/src/relay.rs:674-676`):
```json
{"kind":41010,"created_at":<now>,"content":"",
 "tags":[["p","<other1>"],...,["p","<otherN>"],["client","<random uuid v4>"]]}
```
Rules: 1–8 **other** pubkeys, distinct, 64 lowercase hex, viewer excluded
(`src/features/relay/direct-message-policy.ts:1-12`). The `client` tag makes each request a new
event id; required because a duplicate event returns `message:"duplicate: already processed"` with
**no channel_id** (`buzz:crates/buzz-relay/src/handlers/command_executor.rs:333-341`).

Relay (`buzz:crates/buzz-relay/src/handlers/command_executor.rs:294-426`): self + others deduped, ≤9 total; idempotent per participant
set. Reply on success:
```json
{"event_id":"<id>","accepted":true,
 "message":"response:{\"channel_id\":\"<uuid>\",\"created\":true|false}"}
```
Client requires HTTP 200, `event_id == event.id`, `accepted==true`, `message` prefix `response:`,
`channel_id` a lowercase UUID. Side effects:
- new DM: 40099 `{"type":"dm_created","actor":hex,"participants":[hex...]}`, emit 39000/39001/39002,
  44100 to each participant;
- existing DM: clears caller's `hidden_at` (unhide) and republishes caller's 30622.

### 5e.2 DM channel id derivation

Not derived client-side. Server: `participant_hash = SHA256(concat(sorted, deduped 32-byte pubkeys))`
(`buzz:crates/buzz-db/src/store/dm.rs:50-60`); lookup by hash among non-deleted `dm` channels; if absent create
with `id = Uuid::new_v4()`, `name = "DM"` (2 participants) or `"Group DM (<n>)"`,
`visibility=private`, all members `role=member` (`buzz:crates/buzz-db/src/store/dm.rs:358-390,103-220`). So channel id is a random
UUID, stable per participant set (incl. self).

### 5e.3 Post-open confirmation

`src/features/relay/direct-messages.ts:66-102`: after receipt, strong read
`{"kinds":[39000,39002],"consistency":"strong","authors":[relay],"#d":[id],"limit":2}`; then require
`channels.list()` entry with `channelType=="dm"`, not archived, `members` exactly = {viewer} ∪ pubkeys.
Then notify `subscribeOpened` listeners (unhide local). First message is ordinary kind 9 with
`["h",id]` through the durable outbox; navigation waits for accepted/seen.

DM 39000 includes `["hidden"]`, `["private"]`, `["t","dm"]`, `["closed"]`, and all participants as
`["p",hex]` (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1222-1233`). 39001 for DMs has no `p` (no owner/admin).

### 5e.4 Group DMs

Same 41010 with up to 8 others. Membership is immutable: 41011 `[["h",dm],["p",new]...]` creates a
**new** DM for the expanded set (≤9) and returns `response:{"channel_id":...}`
(`buzz:crates/buzz-relay/src/handlers/command_executor.rs:428-563`, builder `buzz:crates/buzz-sdk/src/builders.rs:1887-1892`). The app never sends
41011 (`src/features/channel-members/members.ts:18` "never offer … DM expansion"); `canAddMembers`
requires stream/forum.

### 5e.5 Hide DM (41012) and visibility (30622)

Hide command (`src/features/relay/channel-lifecycle-protocol.ts:23-48`):
`{"kind":41012,"content":"","tags":[["h","<dm uuid>"]]}`, only if lifecycle settings `canHide`
(type `dm`). Relay (`buzz:crates/buzz-relay/src/handlers/command_executor.rs:565-636`): caller must be member, channel type dm; sets
`hidden_at=NOW()`; republishes 30622; reply `message:"{}"`.

30622 (relay-signed, `buzz:crates/buzz-relay/src/handlers/side_effects.rs:3636-3718`; kind `buzz:crates/buzz-core/src/kind.rs:451`):
```json
{"kind":30622,"content":"","tags":[["d","<viewer>"],["p","<viewer>"],["h","<dm1>"],["h","<dm2>"]]}
```
Read (`channel-lifecycle.ts:107-131,219-251`):
`{"kinds":[30622],"authors":[relay],"#d":[viewer],"#p":[viewer],"limit":1}` (+`consistency:"strong"`
on confirmation). Validate: exactly one `p`==viewer, content `""`, each `h` is a 2-element UUID tag.
Keep newest; absence => empty set. Hide confirmation polls at delays `[0,250,750,1500]` ms until
the id appears. Visibility only filters sidebar rows; exact navigation still works. Unhide = re-open
via 41010 (server clears hidden_at); no in-app unhide UI (`docs/channels.md:481-488`).

### 5e.6 People picker

`direct-messages.ts:27-65`, `src/features/direct-messages/usePeople.ts:147`: kind-0 reads,
`{"kinds":[0],"limit":L,"page":P, ("authors":[hex] | "search":q,"search_mode":"prefix")?}`;
browse page 1 limit 15, later pages 30 (relay page = page-1); search 30. Excludes viewer.
Remote search debounced 150 ms. Query ≤100 chars.

### 5e.7 DM policy summary

- Lifecycle: DMs have no archive/delete/leave; Hide only (`channel-lifecycle-protocol.ts:165-178`).
- No metadata edit (`canEdit` false for dm), no member administration (`administration-protocol.ts:90-99`).
- DM previews in sidebar for 1:1 use participant avatar (`ChannelSidebar.tsx:261-275`).

---

## 5f. Profiles

### 5f.1 Kind 0 parsing (`src/features/relay/profiles.ts:7-73`)

- Only the latest kind 0 per pubkey counts. `newer()` uses higher `created_at`, then lower id on ties (docs/profiles.md "Agent identity").
- JSON fields read: `display_name`, `name`, `picture`, `about`, `nip05`, `is_agent`/`isAgent`.
  - `name` = first non-blank of `display_name`, `name`. Otherwise `pubkey.slice(0,10)` with `nameIsFallback: true`.
  - `picture` -> `avatarSource()` (`src/shared/avatar-source.ts:2-18`). Accepted: `https:` URL without credentials, ≤2048 chars, or `data:image/(png|jpeg|webp|gif);base64,…` ≤512 KiB. Anything else is dropped (initials fallback). Remote avatars are displayed through the session's authenticated media proxy at size `small` (`avatar-source.ts:22-30`).
  - `about`, `nip05`: trimmed, omitted if blank.
  - `isAgent: true` if `is_agent === true || isAgent === true`, **or** if the event has a NIP-OA tag `["auth", <owner hex64>, <conditions>, <sig hex128>]` (exactly 4 elements). That tag also sets `ownerPubkey`. The fold checks shape only. Signature verification of the owner attestation happens elsewhere (`useAgentOwnerEvidence.ts`). The preimage is `"nostr:agent-auth:" + agent_hex + ":" + conditions` (`buzz:crates/buzz-sdk/src/nip_oa.rs:1-17`).
  - Invalid JSON gives a fallback name plus agent info from the auth tag.
- Profile type: `{name, nameIsFallback?, picture?, about?, nip05?, isAgent?, ownerPubkey?}`.

### 5f.2 Kind 10100 agent metadata

- The relay acts on `channel_add_policy` (string, required): it stores the per-user policy (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1304-1336`). CLI writes `{"channel_add_policy": "<policy>"}` with no tags (`buzz:crates/buzz-cli/src/commands/channels.rs:1808-1818`).
- App display (`src/features/agents/public-metadata.ts:44-91`): the latest 10100 by the agent pubkey. Fields: `agent_type` (string, default `"agent"`) and `capabilities` (string[]). Validation also rejects mixed arrays in `channels`/`channel_ids`, a non-string `owner_pubkey`, `respond_to` outside `owner-only|allowlist|anyone`, and a non-string[] `respond_to_allowlist`. **If a kind 30177 managed-agent policy from the verified owner** (`["d", agentPubkey]`) exists, it wins and yields `{agentType:"agent", capabilities:[]}`.
- Ownership never comes from 10100. It comes only from the kind-0 NIP-OA `auth` tag.
- Fetch: `{kinds:[10100], authors:[pk], limit:1}` plus, when the owner is known, `{kinds:[30177], authors:[owner], "#d":[pk], limit:1}` (`src/bundled/profiles/ProfilePublicMetadata.tsx:47-53`).

### 5f.3 Fetch strategy (`src/features/relay/profile-directory.ts`)

- `ensure(ids, priority="foreground")` fetches only ids **not already in the snapshot** and not in flight. It chunks into **500 authors** per read: `{kinds:[0], authors, limit:500}` (`profile-directory.ts:76-108`). More than 1024 unique ids in one call throws.
- Results are filtered to the requested authors. In-flight reads are de-duplicated per id.
- Thread and channel enrichment runs in the background. The profile panel fetches its single key with retry (docs/profiles.md "Boundaries").
- **Live**: the global `profiles` route on the same socket sends `REQ` `{kinds:[0,10100,30177], since, limit:500}`, plus `{kinds:[30315], "#d":["general"], since, limit:500}` and `{kinds:[30030], "#d":["buzz:custom-emoji"], since, limit:500}` (`src/features/relay/live.ts:579-633`; `LIVE_REPLAY_LIMIT = 500` `live.ts:19`). These have no author filter: the relay pushes all profile changes in the community.
- Other direct reads: own profile `{kinds:[0], authors:[viewer], limit:5}` (`src/app/shell/ProfileButton.tsx:147`); member search `{kinds:[0], search:q, limit:100}` (`src/buzz-mcp/tools.ts:274`, `src/features/channel-members/search.ts:30`).

### 5f.4 Caching and staleness

- Storage: `ByteLru<RelayEvent>(1024 entries, 2 MiB)` keyed by pubkey, holding the winning signed event (`profile-directory.ts:25`). The display snapshot is derived by `foldProfiles` over the retained events plus local outbox kind-0 events that are not `failed` (optimistic own profile, `profile-directory.ts:44-75`).
- **No TTL.** Freshness comes from (a) live route pushes and (b) `reconnect()`, which re-reads every retained and pending id with `refresh=true` at background priority, without dropping current heads, so a lagging read cannot roll back (`profile-directory.ts:144-151`). `clear()` (access/cache invalidation) drops everything.
- `selectProfiles(queries, ids)` is a narrow projection that notifies only when one of the referenced ids' value object changes (`src/features/relay/profile-selection.ts:5-34`). The directory keeps object identity stable when fields are unchanged (`profile-directory.ts:55-67`).

### 5f.5 Publishing own profile

Native path (`src/features/communities/native-api.ts:184-224`):
- Input validation: `name` non-blank and ≤100 chars. `picture` empty or an HTTPS URL ≤2048 with no credentials (`src/features/profiles/avatar-upload.ts:10-17`). `about` ≤500.
- Content = `JSON.stringify({...existingKind0Fields, name, display_name: name, picture, about})`. It preserves unknown existing keys and sets `name` and `display_name` to the same trimmed value. UTF-8 size must be ≤16000 bytes.
- Event: `{kind:0, tags:[], content, created_at:now}`. It is signed and sent as `POST /events`. Response must have `event_id == id` and `accepted === true`.
- Relay side effect syncs `display_name` (or `name`) and other fields to the users table. Absent fields are cleared (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1338-1360`). The SDK `build_profile` writes only the present fields among `display_name,name,picture,about,nip05` (`buzz:crates/buzz-sdk/src/builders.rs:743-768`).
- Avatar upload: upload to relay media (see media section), then put the HTTPS URL in `picture`.

### 5f.6 NIP-05

Parsed and shown as **self-declared, not DNS-verified** (docs/profiles.md lines ~133-140, ~208). The relay serves `GET /.well-known/nostr.json` (`buzz:crates/buzz-relay/src/router.rs:320`). The app makes no NIP-05 verification call.

---

## 5g. Members / roster

### 5g.1 Channel roster 39002 and admins 39001

39002 (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1117-1160`): content `""`, tags
`[["d","<uuid>"],["p","<hex>","","<role>"],...]`, role ∈ `owner|admin|member|guest|bot`
(`buzz:crates/buzz-core/src/channel.rs:108-136`). Relay may include its own pubkey (moderation DMs).
39001 (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1267-1286`): `[["d",uuid],["p",hex,"owner"|"admin"],...]` for elevated members only.

Client validation (`administration-protocol.ts:26-100`, `channel-lifecycle-protocol.ts:110-152`):
- each `p` entry length 2..4, valid 64 hex, unique; role from index 3 (unknown => `"unknown"`).
- 39001 entries must be length 3, role owner|admin, unique, **and match the 39002 role**; every
  owner/admin in 39002 must appear in 39001 => else "Member roles are inconsistent".
- Reads always exact: `[39000,39001,39002].map(k => {"kinds":[k],"consistency":"strong",
  "authors":[relay],"#d":[id],"limit":1})`, fresh, foreground (`administration.ts:72-91`).

`MemberAuthority`: `canManage` = active channel (not dm, not archived, not session) && viewer in
39001; `canRemoveOwnedAgent` = active && viewer has known role. `canManageMember(key)` = canManage &&
key≠viewer && role ∉ {owner, unknown}. Removal also allowed for an agent whose verified NIP-OA
owner (kind-0 `auth` attestation) == viewer.

### 5g.2 Add member (9000)

Ordinary invite (`src/features/channel-members/members.ts:90-222`) via durable outbox:
```json
{"kind":9000,"content":"","tags":[["h","<uuid>"],["p","<hex>"],["role","bot"]?]}
```
`["role","bot"]` added when target is a known agent; otherwise no role tag (server keeps existing
role or defaults `member`). Outbox appends `["client-id",uuid]` (`src/features/relay/outbox.ts:734`).
Gate `canAddMembers`: outbox supports 9000, channel not archived, type stream|forum, viewer is
member OR (readOnly && !private) (`members.ts:19-29`). Flow: refresh roster -> reuse pending
unacknowledged 9000 for same h/p -> send -> wait outbox accepted/seen -> refresh roster -> require
target in `members`. Failed items older than 15 min must be dismissed first.

### 5g.3 Role change / remove (administration)

Template (`administration-protocol.ts:164-207`; contract fixtures `administration-contract.json`):
```json
{"kind":9000,"content":"","tags":[["h","<uuid>"],["p","<hex>"],["role","admin"|"member"|"guest"]]}
{"kind":9001,"content":"","tags":[["h","<uuid>"],["p","<hex>"]]}
```
Exactly 3 / 2 tags in that order, target ≠ viewer, no `owner`/`bot` role grants. Native admits only
these (`channel_writes.rs:115-124`). Flow (`administration.ts:170-280`): authorize from fresh read
-> sign -> verify signer didn't alter -> authorize again -> publish (no outbox, no retry) -> strong
re-read must show change (`memberChangeConfirmed`: removed => key absent; else role equal). Uncertain
outcome blocks resubmission until a refresh confirms. One op per channel at a time; 20 s timeout.

Server authz (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:451-560`, `buzz:crates/buzz-relay/src/handlers/channel_authz.rs:111-216`):
- 9000: open channel: any authenticated user; private: actor must be member; elevated role grants
  and any role change of an existing member require owner/admin; cannot demote sole owner; third-party
  add checks target agent's `channel_add_policy`.
- 9001 others: owner/admin, or member who owns the target agent. Self: not sole owner.
- All admin kinds rejected on archived channels except 9002 `archived=false`.
Side effects: 40099 `{"type":"member_joined","actor","target"}` / `{"type":"member_removed"|"member_left",...}`,
re-emit 39000/39001/39002, 44100/44101 to target (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1429-1571`).

### 5g.4 Membership activity rows (40099)

`src/features/relay/membership.ts:6-42`: accept only `kind 40099 && pubkey==relayAuthor`, content
JSON with 64-hex `actor`:
- `member_left` (target absent or == actor) -> target = actor;
- `member_joined` / `member_removed` with 64-hex `target`.
Other 40099 types (`channel_created`, `dm_created`, `topic_changed`, `purpose_changed`,
`visibility_changed`, `ttl_changed`, `channel_archived`, `channel_unarchived`, `channel_deleted`)
are not rendered as membership rows. Channel row kinds = `[9,40002,40008,40099]` (`membership.ts:6`).
Grouping (`src/features/messages/membership-rows.ts:1-132`): newest-anchored; break on non-membership
row, different local day, gap > 3600 s; joins group with joins, leaves with leaves, removals only
with same actor; self-join(s) followed by same person leaving => "joined and left". Text: up to 3
names then "N others"; `you`/`You` for viewer; "added by X" when all additions share an adder.

### 5g.5 Community roster: NIP-43 (13534 etc.)

13534 = relay membership list snapshot (relay-signed, `buzz:crates/buzz-core/src/kind.rs:398`;
built `buzz:crates/buzz-db/src/store/relay_members.rs:1218-1232`):
```json
{"kind":13534,"content":"","tags":[["-"],["member","<hex>","owner"|"admin"|"member"],...]}
```
Read (`src/features/communities/roster.ts:47-59`): `{"kinds":[13534],"authors":[relay],"limit":1}`
fresh; newest by created_at; tags other than `member`/invalid hex ignored; unknown role => member;
no snapshot => `null`. `managesCommunity(role)` = owner|admin. UI hint only.
Related: 8000/8001 deltas `[["-"],["p",hex]]` (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:3285-3345`); 9030 add
`[["p",hex],["role","admin"|"member"]]`, 9031 remove `[["p",hex]]`, 9032 change role (same as 9030)
(`src-tauri/src/relay.rs:325-334`); 28936 leave community `{"content":"","tags":[["-"]]}`
(`src/features/communities/leave-protocol.ts:4-15`). Leave refusals returned verbatim:
`"invalid: you are not a relay member"`, `"invalid: relay membership is not enabled"`,
`"blocked: you are banned from this community"`, `"invalid: relay owner cannot leave"`,
`"community writes are temporarily unavailable"`. 28934/28935 were not found in app or relay kind table.

### 5g.6 Member search

`src/features/channel-members/search.ts:1-59`: page size 30; query ≤256; hex or `npub1` => exact
`{"kinds":[0],"authors":[hex],"limit":30}`; else `{"kinds":[0],"search":q,"search_mode":"prefix",
"page":n,"limit":30}`. Rank by name match tier, then name, then pubkey. `more` = !exact && results≥30.

---

## 5h. Presence and typing

Note: `docs/nips/NIP-PS.md` is **Plugin Signing** (kind 1064). It is unrelated to presence. Presence lives in `src/features/presence/{presence.ts,activity.ts,react.tsx}`, the `src/features/relay/live.ts` publisher, `src/features/relay/transport.ts` (snapshot reads) and `docs/presence.md`.

### 5h.1 States

`"online" | "away" | "offline"` on the wire. The client adds `"unknown"` (no or expired evidence), which is rendered as no badge and never relabeled Offline (`src/features/presence/presence.ts:6`; docs/presence.md). The relay accepts arbitrary strings up to 128 bytes on WS. A JSON `{"status":"…"}` form is accepted as legacy (`buzz:crates/buzz-relay/src/handlers/event.rs:917-933`). The REST enum is the three values (`buzz:crates/buzz-core/src/presence.rs:5-18`).

### 5h.2 Publish (kind 20001, ephemeral)

Exact event the app signs (`src/features/relay/live.ts:921-981`):
```json
{"kind":20001,"content":"online","tags":[],"created_at":1760000000}
```
- **Bare string content and no tags.** The signer output is checked: same pubkey, kind 20001, same content, `tags.length === 0`. SDK `build_presence_update` instead adds `["status", s]` (`buzz:crates/buzz-sdk/src/builders.rs:1898-1913`), which the relay ignores. The relay reads `content`.
- Sent as `["EVENT", ev]` over the **authenticated WebSocket** only. Never over HTTP, never through the outbox, never replayed. Waits up to 10 s for the `OK`. Returns `true/false` (accepted), `null` (local skip), or `{retryAfterMs}`.
- Relay: `offline` -> `clear_presence`. Anything else -> `set_presence` in Redis with **TTL 180 s** (`buzz:crates/buzz-pubsub/src/presence.rs:16`, `:27-44`). The event is then fanned out live. If Redis fails, the relay sends `OK false` with "error: presence storage unavailable" (`buzz:crates/buzz-relay/src/handlers/event.rs:935-976`).
- **Cadence** (`src/features/presence/presence.ts:226-293`):
  - Renewal every `60_000 + rand*5_000` ms (`presence.ts:14`).
  - First send after connect or lock handoff: 250–1000 ms jitter. Immediately (0 ms) on a status change or explicit command.
  - Local skip (`null`): retry in 5–6 s. Gate: `retryAfterMs + 50` ms.
  - Accepted `offline` stops renewal (offline is a clear, not a lease). Offline is never published on close.
  - Shared send gate: at most one presence publish per **5 s** per host (`live.ts:26-55`, `presenceSent` sets +5000 ms).
  - One publisher per (relay scope, viewer) across windows, via Web Lock `buzz-presence:<partition>`.
- **Automatic status** (`src/features/presence/activity.ts:55-90`): `away` after **600_000 ms (10 min)** without Buzz input. On Tauri, OS idle is sampled every 30 s (`get_os_idle_seconds`). Manual overrides `auto|away|offline` persist in localStorage `buzz-presence.v1:<pubkey>` (docs/presence.md).

### 5h.3 Read: HTTP snapshot

Native (direct relay) path (`src/features/relay/transport.ts:1133-1170`):
- `POST {httpOrigin}/query`, NIP-98-signed through the same `signer.request` as other reads (`transport.ts:1263-1287`).
- Body is exactly one filter: `[{"kinds":[20001],"authors":[...≤256 unique hex],"limit":<authors.length>}]`. Validated by `presenceFilter`: keys exactly `authors,kinds,limit` (`src/features/relay/http-admission.ts:267-285`).
- Response: JSON array of **relay-signed** kind 20001 events, one per subject that has a live Redis entry. `created_at = now`.
  ```json
  [{"id":"…","pubkey":"<relay NIP-11 self>","kind":20001,"created_at":1760000000,"content":"away","tags":[["p","<subject>"]],"sig":"…"}]
  ```
  The relay synthesizes this from Redis when every filter is kind 20001 or 40902 with authors (`buzz:crates/buzz-relay/src/api/bridge.rs:2560-2647`). **Empty array = authoritative "all offline"**. A Redis failure returns an HTTP error, not `[]`.
- Client validation (`transport.ts:307-345`, `parsePresence`): array length ≤ requested. Each event must be kind 20001, signed by the relay author, with exactly one 2-element `p` tag naming a requested, not-yet-seen subject. Otherwise the **whole snapshot** is rejected ("Untrusted presence snapshot") and every subject becomes unknown. Unsupported status strings become `unknown` for that subject only. Requested subjects not in the response are set to **offline**.
- Bounds: response ≤1 MiB (`http-admission.ts:286-305`), request lifetime 10 s, 20 KiB request (docs/presence.md).
- Dev broker path is `POST {endpoint}/presence-snapshot` with the same body. HTTP 204 means a local skip (`transport.ts:476-512`). It is not relevant to a native client.

### 5h.4 Read: live

A single demand-scoped live-only route on the existing socket: `REQ` `{kinds:[20001], authors:[…], since, limit:0}` (`live.ts:578-579`, `live.ts:909-920`). Incoming events are applied only if the pubkey is in the current author set. A `{"status":…}` JSON content is also parsed (`presence.ts:46-67`).

### 5h.5 Demand, TTLs, scheduling (`presence.ts`)

- At most **256** subjects. Priority order: viewer first, then mounted profile panels, then existing selections, then acquisition order (`presence.ts:90-120`).
- New demand coalesces for **100 ms**. Reads are gated to start ≥5 s after the previous read settles. Successful snapshots refresh after **60–65 s**. Network failure: wait `max(60–65 s, server retryAfterMs)`.
- Evidence (snapshot or live) expires **75 s** after receipt, then becomes `unknown` (`presence.ts:59`, `:173`, `:266`).
- Reads and observation happen only when connected and the window is visible. Hidden views drop observation. Renewal continues while hidden.
- Relay-side Redis lease is 180 s, so a crashed client can appear online for up to 3 min.

### 5h.6 Typing (kind 20002, ephemeral)

**The buzz-app never publishes typing indicators. It only consumes them** (grep: no writer of 20002 in `src/`). Agents (buzz-acp) publish them:
```json
{"kind":20002,"content":"","tags":[["h","<uuid>"],["e","<root>","","root"],["e","<parent>","","reply"]]}
```
The `e` tags are present only inside a thread. The root tag is omitted when root == parent (`buzz:crates/buzz-acp/src/relay.rs:1011-1039`). ACP refreshes every **3 s** while working (`buzz:crates/buzz-acp/src/lib.rs:3096-3104`). The relay checks channel membership for ephemeral events with `h` (`buzz:crates/buzz-relay/src/handlers/event.rs:979+`).

Receive rules (`src/features/relay/typing.ts`):
- Arrives on channel live routes (20002 is in `CHANNEL_KINDS`). It is dropped if it has no `h` (`live.ts:844`). It never enters history, unread or caches (`session.ts:536-551`).
- The event is ignored if it is from the viewer, or if `created_at*1000 > now` or older than **8 s** (`ACTIVITY_LIFETIME_MS = 8_000`, `typing.ts:4`). It is also ignored if it does not have exactly one `h` matching `[a-zA-Z0-9_-]{1,128}` with access. Thread scope: if `e` tags exist, there must be ≤2, all hex64 with marker `root|reply`, exactly one `reply` and ≤1 `root` (`typing.ts:89-123`).
- Key: `channelId:threadRootId:pubkey`, at most **1024** records (`typing.ts:6`, `:125-141`).
- Visible until `pulse + 8 s`. When a real message (kind 9/40002/40008) from the same key arrives with `at ≥ lastActivity`, the indicator hides immediately and new pulses are suppressed for **2 s** (`POST_MESSAGE_QUIET_MS = 2_000`, `typing.ts:5`, `:78-88`).
- If a native client chooses to *send* typing, mirror the ACP shape: empty content, `h` plus optional NIP-10 `e` tags, about every 3 s while composing. This is a recommendation; the app has no such behavior.
- Agent "working" pill also consumes 20002 from known agents (`src/features/agents/activity.ts:620-670`).

---

## 5i. Read state (NIP-RS) and unread computation

### 5i.1 Read-state event (wire)

Source: `src/features/relay/read-state-model.ts:59-74`, `src-tauri/src/identity.rs:573-600,766-880`, `src-tauri/src/relay.rs:527-562`.

```json
{
  "kind": 30078,
  "pubkey": "<viewer hex>",
  "created_at": 1760000000,
  "tags": [["d", "read-state:<slot>"], ["t", "read-state"]],
  "content": "<NIP-44 v2 ciphertext, base64>",
  "id": "...", "sig": "..."
}
```

- `slot` = 32 lowercase hex chars, regex `^[0-9a-f]{32}$`. Generated once per local journal as `crypto.randomUUID().replaceAll("-", "")` (`read-state-storage.ts:47-58`). One slot per device/browser-profile per (community, viewer) partition.
- Recognition (`readCoordinate`, `read-state-model.ts:59-74`): kind == 30078 AND exactly one `d` tag AND exactly one `["t","read-state"]` tag AND `d` matches `^read-state:[0-9a-f]{32}$`. Anything else is not read state (kind 30078 is shared with sidebar prefs `channel-sections|channel-stars|channel-mutes|channel-sort` and channel kits; the `d` value selects the contract, `src-tauri/src/relay.rs:586`, `identity.rs:566-571`).
- No `h` tag. Server treats 30078 as global-only, parameterized-replaceable on (pubkey, kind, d), write scope `UsersWrite` (`buzz:crates/buzz-relay/src/handlers/ingest.rs:503,694`).
- Signing produces exactly the two tags above in that order (`identity.rs:869-878`).

**Encryption** (`identity.rs:832-880` sign, `766-830` decode):
- NIP-44 **v2**, self-encrypted: `nip44::encrypt(secret, own_pubkey, plaintext, V2)`. Decode derives `ConversationKey::derive(secret, own_pubkey)` once per batch and decrypts each.
- Decode host checks (all-or-nothing, any failure = error for the whole batch): ≤16 events per call and ≤512 KiB serialized batch; each raw event ≤96 KiB serialized; raw tag arrays inspected before deserialization (exactly 1 `d`, exactly 1 `t=read-state`, valid coordinate); signature verified; `kind==30078` and `pubkey==viewer`; plaintext ≤128 KiB; JSON parses; `validate_read_blob` passes. Returns `[{eventId, blob}]`.
- Sign host checks: valid slot; `created_at ≤ u32::MAX` and `|now - created_at| ≤ 60` s; `validate_read_blob`; plaintext ≤ **40 KiB**. Signed event via ordinary `key.sign` (64 KiB event bound, `identity.rs:883-885`).
- Publish (`relay.rs:548-562`): serialized event ≤64 KiB, re-run `decode_read_state([event])` (proves own, well-formed, decryptable), then NIP-98-authenticated `POST /events` with the exact bytes. The generic `relay_sign` refuses kind 30078 (`relay.rs:235-237`), so read state can only be signed via the dedicated path.
- Client decodes in batches of **4** events (`read-state.ts:301-316`) and re-checks that the returned `eventId`s match the batch 1:1.

### 5i.2 Plaintext payload schema (v1)

Source: `read-state-model.ts:43-133,225-258`, `identity.rs:582-600`.

```json
{
  "v": 1,
  "client_id": "6f1c0e8a-...-uuid",
  "contexts": {
    "<channelId>": 1760000000,
    "thread:<rootEventIdHex>": 1760000100,
    "msg:<eventIdHex>": 1760000200,
    "activity:<channelId>": 1760000300,
    "thread-activity:<rootEventIdHex>": 1760000400,
    "esc:ov_weird": 1760000000,
    "ov_s:<ctx>": 3, "ov_c:<ctx>": 2, "ov_b:<ctx>": 1760000000
  }
}
```

| Field | Type | Rule |
|---|---|---|
| `v` | int | must be `1`; any other value fails closed ("Unsupported read-state blob") |
| `client_id` | string | 1..64 Unicode code points. Locally `crypto.randomUUID()` (`read-state-storage.ts:51`) |
| `contexts` | object<string,uint32> | ≤10,000 entries (`READ_STATE_KEYS`); whole blob JSON ≤128 KiB on parse |

Context-key grammar (`contextId`: non-empty, ≤256 UTF-8 bytes; `read-state-model.ts:17-18`):
- Frontier keys (value = "read through this `created_at`"):
  - `<channelId>` — channel id must match `^[a-zA-Z0-9_-]{1,128}$` (`targetKey`, `read-state-model.ts:26-33`).
  - `thread:<64hex>` — thread root.
  - `msg:<64hex>` — individual message.
  - `activity:<channelId>` — channel bottom catch-up (Buzz-desktop extension; older desktop/mobile clients ignore it, `docs/unread.md:362-366`).
  - `thread-activity:<rootId>` — thread bottom catch-up.
- Escaping: any frontier key starting with `ov_` or `esc:` is written as `esc:<key>`; parser strips one `esc:` prefix (`read-state-model.ts:53-56`).
- Override registers (manual-unread from other clients): triplets `ov_s:<ctx>` (set counter), `ov_c:<ctx>` (clear counter), `ov_b:<ctx>` (baseline frontier). Parse rule (`:95-128`): group by suffix after the 5-char prefix; accept a group if it is exactly `{ov_c}` alone (→ `{set:0, clear:c, baseline:0}`) or all three present with uint32 values; otherwise drop. Other `ov_*` keys and non-uint32 frontier values are silently dropped. Duplicate frontier keys (escaped vs not) merge by max.

Parsed in-memory form:
```ts
type Override = { set: number; clear: number; baseline: number };
type ReadState = { frontiers: Record<string, number>; overrides: Record<string, Override> };
```

### 5i.3 Merge rules (exact)

Source: `read-state-model.ts:144-202`, `unread.ts:526-556`.

```text
merge(states...):                       # commutative, idempotent, monotone
  frontiers[k]  = max over states
  overrides[k]  = { set: max(set), clear: max(clear), baseline: max(baseline) }
  if |frontiers| + 3*|overrides| > 10000: error "capacity reached; saved intent retained"

effectiveFrontier(state, key, channelId?, rootId?) =
  max of defined { frontiers[key], frontiers[channelId], frontiers["thread:"+rootId] }   # undefined if none

overrideActive(ov, frontier) = ov && ov.set > 0 && ov.set > ov.clear && (frontier ?? 0) <= ov.baseline
```

- All slots (this device's and every other device's) are merged with `max`; no per-slot precedence, no last-writer-wins. Nothing can rewind a frontier.
- Encoding for publish (`readBlob`, `:225-258`): frontiers sorted by key (`localeCompare`), escaped; each override, if `overrideActive(ov, effectiveFrontier(state,key))` emits all three `ov_s/ov_c/ov_b`, else if `set||clear` emits only `ov_c:<k> = max(set, clear)` (permanent clear floor). Result re-parsed as a self-check; JSON must be ≤40 KiB (`READ_STATE_PLAINTEXT_BYTES`).
- `changeOverride` (`:204-223`) exists but **has no caller** in non-test code. This client never creates overrides; it only reads them. **Publication of any state that contains overrides is blocked** (`read-state.ts:531-533`: "Synchronized override publication is not enabled"). Consequence: once a peer slot publishes an override, this device's merged journal contains it and further publishes fail until it's gone (not found anywhere that clears it).
- Version clock `readVersion(now, previous)` = `max(now, previous+1)`; error if `> now+60` or not uint32 (`:260-267`). `previous = max(journal.lastCreatedAt, ownSlotEvent.created_at)`.

### 5i.4 Local journal (persistence)

Source: `read-state-storage.ts:19-58,197-281`, `docs/unread.md:310-319`.

IndexedDB DB `buzz-read-state-v1` v1, store `partitions`, key = partition scope (`transportPartition(transport)` = relay/community + viewer; `session.ts:834-849`). Every mutation is a `durability:"strict"` readwrite transaction that re-reads the current record (cross-window merge). Corrupt record = hard error, never treated as empty.

```ts
type ReadJournal = {
  version: 1;
  slot: string;              // 32 hex
  clientId: string;          // uuid
  state: ReadState;          // the SYNC set (eligible for publication)
  localUnread: Record<string, number>; // key -> revision when marked; local-only manual unread
  recent?: Record<string, number>;     // key -> revision of last local interaction (LRU order)
  reserve?: Record<string, number>;    // local-only frontiers evicted from `state`
  homes?: Record<string, string>;      // messageId -> channelId for msg:/thread:/thread-activity: marks
  revision: number;          // local mutation counter
  acceptedRevision: number;  // highest revision reconciled with relay
  lastCreatedAt: number;     // last signed created_at
  pending?: { event: RelayEvent; revision: number }; // exact signed bytes awaiting publish
};
```
Validation limits: `localUnread` ≤10,000 keys, values in `(0, revision]`; `recent` keys must exist in `state.frontiers`; `reserve` ≤5,000 keys / ≤512 KiB JSON, disjoint from `state.frontiers`; `pending.event` must be own coordinate and `created_at ≤ lastCreatedAt` (`:60-196`).
Effective local state for unread decisions = `{frontiers: {...reserve, ...state.frontiers}, overrides: state.overrides}` (`read-state.ts:84-91`).
`localUnread` key forms: `<channelId>`, `thread:<root>`, `msg:<id>`, `message-force:<channelId>` (`unread.ts:237`).
On community leave: `purgeReadStateStorage(scope)` deletes the partition (`read-state-storage.ts:211-230`).

Multi-window: `BroadcastChannel("buzz-read-state:<scope>")` posts `"changed"` after each save; receivers reload the journal (`read-state.ts:156-159,253-263`). Publisher serialized by Web Lock `buzz-read-state:<scope>` exclusive (`:56-66`). Native port: one process, so a mutex suffices.

### 5i.5 Retention / compaction

Source: `read-state-retention.ts:288-606`, `docs/unread.md:342-411`.

Budgets: journal sync set **96 KiB** serialized blob (`retainRead` default `maxBytes`), wire publish **40 KiB**, both ≤10,000 keys; reserve 5,000 keys / 512 KiB.

`retainRead(states, recent, clientId, maxBytes, covered?)` pseudo-code:
```text
frontiers = max-merge of all states' frontiers; overrides = merge of all overrides
used = bytes('{"v":1,"client_id":"<id>","contexts":{}}'); keys = 0
cost(key,v) = bytes(JSON.stringify(key)) + 2 + len(str(v))
1. For every override: take ov_s/ov_c/ov_b (+ its direct frontier if any). Failure -> throw (never truncate floors).
2. If ANY override exists: take every non-"msg:" frontier (inherited ancestry protection). Failure -> throw.
   Also disable coverage pruning.
3. scope(k) = 0 channel (no ':'), 1 "thread:", 2 "activity:"/"thread-activity:", 3 other (msg:)
   byUse = recent[key] desc, then value desc, then key asc
   select():
     for k in frontiers sorted by (scope, byUse): take with cap = 0.75*maxBytes and 0.75*10000 keys; STOP at first miss
     for k in frontiers sorted by byUse: take if fits full budget (skip misses)
4. Coverage pruning loop (only when no overrides and `covered` provided):
     repeat: for each kept key, cover = covered(key) restricted to kept keys;
             resolve cover chains; drop keys whose final cover is kept and not itself covered;
             refund bytes; recent[cover] = max(recent[cover], recent[dropped]); select() again
     until nothing drops
5. return kept frontiers + all overrides; recent filtered to kept keys
```

Coverage function (`unread.ts:590-624`), returns the covering key or undefined:
- `activity:<c>` covered by `<c>` if `frontier(<c>) >= value`.
- `thread-activity:<r>` covered by `thread:<r>` if `>= value`.
- `msg:<id>`: channel = retained evidence channel or `homes[id]`; covered by `<channel>` if `frontier(channel) >= event.created_at` (or the mark value if event not loaded); else by `activity:<channel>` only if event is loaded, channel type known, message is "timeline" (top-level, non-priority, non-DM) and `activity` frontier ≥ created_at.
- `thread:<r>` / `thread-activity:<r>`: covered by `<channel>` if `>= value` (channel via evidence or homes).
- Thread marks never cover `msg:` marks.

`retainLocalRead` (`:525-606`): runs `retainRead` for the journal; whatever doesn't fit goes to `reserve` sorted by (protected-by-override first, scope, value desc, key); skips redundant (covered) entries; stops at 5,000 keys / 512 KiB. Keys returning from reserve take max value.

Publication runs a second `retainReadState([journal.state], recent, clientId, 40 KiB, covered)` — the wire copy is a smaller subset of the journal (`read-state.ts:534-540`).

`homes` discovery (`unread.ts:1557-1632`): once per session after evidence repair, for `msg:/thread:/thread-activity:` marks with 64-hex id, value ≤ max channel frontier, unknown home, not loaded: query `{ids:[≤100], kinds:[9,40002,40008], limit:n}` background; save found channels via `learnHomes` (no publish). Skipped entirely if overrides exist.

### 5i.6 Sync lifecycle: fetch, live, publish

Source: `read-state.ts:105-855`, `read-state-snapshot.ts`, `live.ts:600-608`, `session.ts:556-560,2523`.

Capabilities: `unsupported` (no host decode) / `read-only` (no sign/publish/lock) / `frontier-sync` (`read-state.ts:146-150`). Status: `loading|local|pending|accepted|reconciled|stale|error`; completeness `unknown|snapshot|bounded`.

**Startup fetch** (`refresh`, `:356-409`; called via `ensure()` first with priority `foreground`, 10 s timeout):
- If NIP-11 advertises `read_state_snapshot` AND reader supports it → **complete snapshot**:
  - NIP-11 descriptor (server `buzz:crates/buzz-relay/src/nip11.rs:408-413`): `{"version":1,"community_id":"<uuid>","max_events":4096,"max_event_array_bytes":8388608}`. Client accepts only `version===1` and UUID-shaped `community_id` (`read-state-snapshot.ts:9-20`).
  - Request: NIP-98-signed HTTP `POST /query` body `[{"kinds":[30078],"authors":["<viewer>"],"read_state_snapshot":1}]` — exact shape, no extra keys (`read-state-snapshot.ts:21-38`; server rejects anything else with 400, `bridge/read_state_snapshot.rs:16-33`).
  - Response: `{"read_state_snapshot":1,"complete":true,"community_id":"<uuid>","pubkey":"<viewer>","snapshot_id":"<64hex>","events":[...]}`. Contains **all** of the author's current kind-30078 events (incl. sidebar prefs); client checks every event is viewer+30078 and `d` unique; ≤4096 events, events JSON ≤8 MiB; body stream capped at 8 MiB + 4 KiB before parsing (`read-state-snapshot.ts:40-100`). Server errors: 413 too large, 503 unavailable.
  - completeness = `snapshot`.
- Else bounded: `reader.read([{kinds:[30078], authors:[viewer], "#t":["read-state"], limit:500}])`; completeness = `bounded`.
- `ingest`: filter recognized own coordinates, keep newest per coordinate (`newer`), decode, then in one storage transaction `retainMarks(current, [current.state, ...decoded])` (i.e. max-merge + retention). Track `slots` map (coordinate → newest event+parsed); >4096 slots → error.
- Merging remote state does not bump `revision` (no republish just because peers changed).

**Live**: the `membership` live route REQ adds `{kinds:[30078], authors:[viewer], "#t":["read-state"], since, limit:500}` (`live.ts:600-608`, `LIVE_REPLAY_LIMIT=500` `live.ts:19`). Every verified event batch from any reader/live path goes through `reads.accept(visible)` (`session.ts:559`) → `ingest` if any event is an own read-state coordinate (`read-state.ts:817-839`). On reconnect: `unread.reconnect()` → `reads.refresh()` then evidence repair in background (`unread.ts:2312-2314`).

**Mutations** (`mutate`, `:410-490`; `readMessages`, `:722-814`): queued serially; one strict transaction: validity fence, `revision+1`, `recent[key]=revision`, merge `{key: timestamp}`, run `retainMarks`, adjust `localUnread`. Local-only changes (`timestamp===undefined`) advance `acceptedRevision` too if nothing else is pending (so they never trigger publish). Frontier mutations require `frontier-sync`, else throw "Read-state sync unsupported by this host". Result `{operationId: "<slot>:<revision>", durability:"saved", sync:"pending"|"local-only"}`.

**Publish cadence**: `schedule()` arms a single **5000 ms** debounce timer (`debounceMs = 5000`, `:113,284-290`) if `frontier-sync`, none armed, not publishing. No max-wait; each mutation while armed does not reset it (timer already set → return). Flush (`:491-592`), 10 s timeout, under exclusive publisher lock:
```text
reload journal
if journal.pending: publishPending(recover=true)
if revision <= acceptedRevision: return
read own coordinate fresh: {kinds:[30078], authors:[viewer], "#d":["read-state:<slot>"], limit:500}; ingest
if own slot event exists with different client_id: error "slot conflict; publication blocked"
if journal.state.overrides non-empty: error (blocked)
wire = retainReadState([journal.state], recent, clientId, 40KiB, covered)
blob = readBlob(clientId, wire, ...)
createdAt = readVersion(now, max(lastCreatedAt, ownEvent.created_at))
event = host.sign({slot, createdAt, blob})
verify event pubkey/coordinate/created_at; decode(event) and compare state == parse(blob) (signer can't swap payload)
save {lastCreatedAt: createdAt, pending: {event, revision}}     # exact bytes persisted BEFORE send
publishPending()
```
`publishPending` (`:593-670`): `host.publish(event)` → POST `/events`; then re-read own coordinate (fresh) and ingest; "observed" iff `merge(own.parsed, payload) == own.parsed`. If observed: clear `pending`, `acceptedRevision = max(acceptedRevision, pending.revision)`, status `reconciled` (or `pending` if newer revisions exist). If not observed: error "accepted; coordinate observation still pending" (retry later, same bytes).
Timestamp refusal recovery: server rejects `|created_at - now| > 900 s` with exact `"invalid: event timestamp too far from server time"` (`buzz:crates/buzz-relay/src/handlers/ingest.rs:2450-2456`). Client maps HTTP 400 (or 503 with `sent:false`) carrying that exact string to `ReadStateTimestampRejected` (`transport.ts:1289-1310`). Only during `recover=true` and only if `pending.event.created_at < now-60`: merge its plaintext into the journal (keep envelope) and let the same flush sign a replacement. At most one replacement per flush. Other errors: keep exact bytes for retry. Failed revision waits for explicit `retrySync()` / next mutation (`attemptedRevision`, `failedRevision`).

Startup with a saved `pending`: `ready` sets status `pending` but does not schedule; the first `refresh()` schedules the flush (`:235-252,398-399`).

### 5i.7 Unread computation (`unread.ts`)

**Evidence store** (`unread.ts:216-260,1665-1706`):
- Retained events: kinds `9, 40002, 40008` (content) + `40003, 5, 9005` (auxiliary). Bound **4096 events / 8 MiB** (JSON bytes). Overflow → clear all evidence, `error="Unread observation capacity reached; refresh available"`, freshness `stale`, stop repair.
- Channel of an event = sole `h` tag value (exactly one `h` tag required; `:142-148`). Auxiliary events inherit channels through `e` targets recursively (≤32 depth, fail-closed; `:153-186`).
- Accepted only if every owning channel is "allowed": listed, not `cached`, viewer ∈ `members` (`:261-269`).
- Fed from every verified read and live batch (`session.ts:559-560`) plus restored disk cache (`session.ts:718`).

**Evidence repair** (= how counts for unloaded channels are obtained; `:1492-1556`): no server-side count API is used. After `reads.ensure()`:
```text
ids = roster channels (not cached, viewer is member)
for batch of 128 ids (relay caps #h at 128):
  reader.read([{kinds:[9,40002,40008], "#h": batch, include_aux:true, limit:500}], {priority, timeout 10s each})
  accept(result)            # publishes progressively
freshness="observed"; then findHomes()
```
278 channels → 3 requests. A busy channel can consume a batch's 500-row sample; counts are therefore "observed", not exact. One-shot: failure needs explicit `refresh()` or reconnect. Roster growth from cached→confirmed re-runs repair (`:1179-1209`). (Server has an unused private accessory API `GET/POST /buzz/v1/me/read-state` and `/buzz/v1/me/sidebar`, gated by `buzz_v1` in NIP-11, `buzz:crates/buzz-relay/src/api/buzz_v1/mod.rs:14-23`; the app does not call it. `feed_types` is not used by unread/inbox.)

**Index** (`indexEvidence`, `:390-455`): tombstones = targets of kind 5/9005 `e` tags where `deletionApplies` (same author; or relay-signed message author; or kind 5 with `k` tag == target kind for kinds 9/40002) (`fold.ts:225-243`). Per non-deleted content event:
```ts
Evidence = {
  event, channelId,
  parentId,          // threadReference(event)?.parentId
  threadRootId,      // threadReference(event)?.rootId (marked root, or = parent)
  rootId,            // only for replies: canonical root resolved by walking retained/lookup events (≤32 hops, same channel, content kinds); undefined if chain broken
  mentioned,         // mentionsViewer(event)
  broadcast,         // tag ["broadcast","1"]
}
```
`threadReference` (`thread-reference.ts:4-13`): scan `e` tags with 64-hex value; last `marker=="root"` wins as root, last `marker=="reply"` wins as reply; reply required (a lone root marker is NOT a reply); `rootId = root ?? reply`. Lower-cased.

`mentionsViewer` (`:372-389`): has `["p", viewer]`; except trusted workflow output (kind 9, `pubkey==relayAuthor`, `["buzz:workflow","true"]`, `["buzz:workflow-owner",viewer]`) counts only if also `["buzz:workflow-mention",viewer]`.

Viewer participation sets: `own[id]=channel` for viewer's messages; `joined` = `channel:parentId` viewer replied to; `ownThreads` = `channel:(rootId ?? id)` of every viewer message + lookup witnesses.

**Unread predicate** (`:487-556`):
```text
conversation(e) = e.parentId &&
   (followChoice(channel:threadRootId) if explicit
    else ownThreads∋channel:threadRootId || own[parentId]==channel || joined∋channel:parentId)
relevant(e, dm) = !e.parentId || dm || e.mentioned || e.broadcast || conversation(e)
category(e, dm) = dm ? "direct" : e.mentioned ? "mention" : conversation(e) ? "thread" : none
priority(e, dm) = category(e,dm) != none || e.broadcast
timeline(e, dm) = !threadReference(e.event) && !priority(e, dm)

isUnread(e, state, dm):
  if e.event.pubkey == viewer: false                       # own messages never unread
  if forcedMessages[channel] ∋ id || localUnread["msg:"+id]: true
  return relevant(e,dm) && afterFrontier(e,state,dm)

afterFrontier(e, state, dm):
  f = effectiveFrontier(state, "msg:"+id, channelId, e.rootId)
  forced = overrideActive(ov["msg:"+id], f)
        || overrideActive(ov[channelId], effectiveFrontier(state, channelId))
        || (e.rootId && overrideActive(ov["thread:"+rootId], effectiveFrontier(state,"thread:"+rootId,channelId)))
  ordinary = timeline(e,dm) ? frontiers["activity:"+channelId] : none
  thread   = e.rootId ? frontiers["thread-activity:"+rootId] : none
  return e.created_at > max(f, ordinary, thread, -1) || forced
```
Notes: a channel frontier covers replies too (inheritance), but `activity:` only covers top-level non-priority messages; mentions/DM/broadcast/thread replies survive channel catch-up. Comparison is strictly `created_at > frontier` (same-second covered).

**Per-target snapshot** (`compute`, `:693-766`):
```ts
UnreadSnapshot = {
  target, latestMessage?: {id, createdAt},   // newest evidence incl. own/read (channel target only); tie -> smaller id
  observedCount: number|null,   // null if channel never had evidence ("known") or not accessible
  attentionCount: number|null,  // subset with priority()
  coverage: "unknown"|"observed", freshness: "unknown"|"observed"|"stale",
  manual: "none"|"local-only"|"remote", error?
}
```
- Target channel: all evidence in channel. Thread: entries with `rootId == target.rootId` (replies only, root excluded). Message: that one event.
- Thread/message targets require the root/message to be retained or lookup-fetched in the same channel; otherwise all-null.
- `manual`: `local-only` if `localUnread[targetKey]`, or message in `forcedMessages`, or channel with `localUnread["message-force:"+c]`; `remote` if override active on key; else `none`.
- Selector cache ≤4096 snapshots (evicts unsubscribed; throws if still full).
- **No caps** (no "99+"); sidebar shows no numerals for non-DM rows (`docs/unread.md:129-142`, `src/bundled/channels/UnreadBadge.tsx:50-97`).

Sidebar presentation (`UnreadBadge.tsx:50-64`):
```text
unread   = manual != none || observedCount > 0
ordinary = manual || (dm ? count>0 : count > attentionCount)     -> bold label
priority = dm || attentionCount > 0                              -> accent dot (only if no thread-activity dot)
threadCount = activity(channel).items.length                     -> thread-activity dot + popover
```
Offscreen "N unread" buttons count distinct offscreen conversations, measured from rendered badges, no extra reads (`docs/unread.md:144-163`).

**Thread activity** (`computeActivity`, `:775-854`): per channel, group unread replies with `rootId` and (mentioned || broadcast || conversation) by `rootId`; item `{channelId, rootId, latestMessageId, authorId, workflowOwnerId, createdAt, preview, unreadCount}`; sorted createdAt desc, then latestMessageId asc. Preview from folded content (edits applied).

**Muted channels**: mute does not affect unread counts, dots, badges or read state (`docs/unread.md:301`, clearing matrix "Mute/Unmute | None | None"; `docs/notifications.md:68-72`). Mute affects notifications only (5l). Mute list lives in kind 30078 `d=channel-mutes` sidebar prefs (`native-sidebar.ts:25`).

**DMs**: channel `channelType==="dm"`. Every DM message is `direct` attention; replies in DMs are relevant. Reading a DM reads it whole (below).

### 5i.8 Conversation-membership lookups (undecided replies)

Source: `unread.ts:1210-1491`, `docs/unread.md:225-289`.
Trigger `want(entry)`: reply, not own, `undecided` (not relevant, no explicit follow choice, no finished lookup for `channel:parent`) AND `afterFrontier` true. Also structural recovery: explicit Follow=true with root not retained → fetch root only (key `channel:root:structure`). Queued; drained in a microtask.
```text
per channel, chunks of ≤50 parents (background priority, 10s):
  A: {kinds:[9,40002,40008], ids:[parents+roots not retained], include_aux:true, limit:n}
  B: viewerReplies: {kinds:[9,40002,40008], authors:[viewer], "#h":[channel], "#e":[parents ∪ roots], include_aux:true, limit:500[, until]}
     if 500 content rows: split parents in half recursively; for 1 parent page back by `until=oldest` up to 10 pages
       until a deciding reply (reply.parent==parent || reply.root==(root??parent)) appears
  failure: requeue, backoff min(60000, 1000*2^failures) ms
  member iff viewer authored parent/root (live, not deleted) or a deciding viewer reply exists
  record Lookup{channelId, done:true, evidence: witnessId, more}
```
Bounds: lookups ≤4096 (LRU), lookup events ≤1024, witnesses ≤4096, retracted ids ≤1024. Fetched events are structural only (never counted). Viewer's own deletions retract witnesses (`:1636-1664`). While queued/running, `attention.pending=true`.

### 5i.9 Thread follows (device-local)

Source: `thread-follows.ts:1-68`, `unread.ts:1849-1876`.
localStorage key `buzz.thread-follows.v1:<partition>`, value JSON array of `[ "<channelId>:<rootIdLowercase>", boolean ]`, newest last, cap **1000** (`THREAD_FOLLOW_LIMIT`). `true` = follow without posting; `false` = unfollow even if participating. Absent = automatic (`ownThreads`). Synced across windows via `storage` event; not synced across devices. Deleted on community leave.

### 5i.10 Marking read

**Automatic viewport dwell** (`src/features/messages/use-reading.ts:29-234`):
- Active iff: element connected, not inside `[inert],[hidden],[aria-hidden=true]`, `settled` (owner finished positioning), document visible, window focused, `document.activeElement` within list / its `[data-reading-surface]` / owning selected tab / its composer, and has layout.
- Re-schedule on scroll, pointerdown, keydown, custom `reading-positioned`, focusin/focusout, window focus/blur, visibilitychange, DOM mutation, resize.
- `visibleIds`: rows `[data-message-id]` (excluding `data-membership-row`) **fully** inside the scroller viewport and window; max 128.
- `bottomId`: only if `scrollHeight - clientHeight - scrollTop <= 1` and the `latestMessageId` row's bottom edge is inside the viewport (tall messages ok).
- Take a lease `unread.reading(channelId)` before the **300 ms** dwell; after 300 ms recompute; `remained` = ids still visible; `caughtUp` = bottom unchanged. Only if `sync().capability === "frontier-sync"`: `catchUp(bottom, rootId)` then `observe(remained)`.

**Lease semantics** (`unread.ts:1877-2068`): ≤64 concurrent leases; invalid after dispose, epoch change, access loss, or newer `localUnread[channelId]` revision.
- `observe(ids)`: per id (verified, in channel, not deleted, not a `msg:` already covered by frontier or by `activity:`) → `reads.read("msg:"+id, event.created_at)`. Does not clear local manual unread.
- `catchUp(id, rootId?)`: key `thread-activity:<root>` or `activity:<channel>`. Abort if retained evidence has a newer top-level/broadcast row (channel) or newer reply in that root (thread). Cut = event.created_at (thread) or max created_at of all channel evidence incl. replies (channel). Skip if `max(frontiers[key], frontiers[channel], frontiers["thread:"+root]) >= cut`. Channel catch-up also clears `localUnread[channelId]`.
- **DM**: any observe/catchUp → `reads.read(channelId, cut=max created_at in DM evidence, explicit=true, clear [channelId])` once per lease; no `activity:`/`msg:` marks.
- Thread panel reading = same hook with `rootId`.

**Explicit actions** (`unread.ts:2069-2267`, matrix `docs/unread.md:291-308`):

| Action | Frontier written | Local-unread cleared |
|---|---|---|
| `markThrough(target, msgId)` | `targetKey(target)` = event.created_at (channel target rejects non-broadcast replies) | that key |
| `markChannelRead(c)` | `<c>` = max(floor(Date.now()/1000) at click, newest retained created_at incl. replies) | `<c>`, `message-force:<c>`, `msg:<id>` for all retained rows, `thread:<root>` for retained reply roots and retained top-level ids |
| same, no evidence, non-frontier host | none | `<c>` keys only (`clearLocalUnread`) |
| `markAllChannelsRead()` | `markChannelRead` per allowed channel with `manual!=none` or any unread row or `thread:` local mark; click time captured once; sequential; first error rethrown after sweep | as above |
| `markMessageRead(c, id)` | `msg:<x>` = created_at for message + loaded reply subtree (excluding own) | those `msg:` keys; `message-force:<c>` if no forced rows remain |
| `markMessageUnread(c, id)` | none; adds subtree to in-memory `forcedMessages[c]` (per visit) + `localUnread["message-force:"+c]` | — |
| `markUnreadLocal(target)` / `clearUnreadLocal(target)` | none (`localUnread[targetKey]` = revision / delete) | — |
| `enterChannel(c)` | clears stale `message-force:<c>` from previous visit | |
| `leaveChannel(c)` | bumps visit counter, drops `forcedMessages[c]` | |

Manual unread is never synced (`syncedManualUnread: false`). Per-channel mutations are serialized (`serialize`, `:243-258`). Opening a channel/inbox row/notification does not itself mark read; dwell does.

### 5i.11 channel-activity.ts (recent sort only)

Source: `channel-activity.ts:1-145`, `native.ts:813-829`, `session.ts:2564-2590`.
Only used when some sidebar section's sort is `"recent"`. Value per channel = newest `created_at` among kinds `[9,40002,40008,45001,45003]`. Refresh: batches of 128 channels, one query per batch with one filter per channel `{kinds:[...], "#h":[c], limit:1}`, 10 s timeout; live verified events can only advance values. Not used for unread.

---

## 5j. User status (kind 30315, NIP-38)

Files: `src/features/relay/user-status.ts`, `user-status-policy.ts`, `src/features/user-status/*`.

### 5j.1 Event shape (`user-status.ts:256-268`)

```json
{
  "kind": 30315,
  "created_at": 1760000000,
  "content": "In a meeting",
  "tags": [
    ["d", "general"],
    ["emoji", "📅"],
    ["expiration", "1760028800"]
  ]
}
```
- `d` is always `"general"`. **No `h` tag.** Events with an `h` tag are rejected on parse (`user-status.ts:17-24`).
- `content` = trimmed text. `["emoji", value]` is **2 elements** (not NIP-30's 3). The value is either a unicode emoji or `:shortcode:`, which is resolved against the community custom-emoji catalog at render time (`src/features/user-status/StatusEmoji.tsx:18-27`). The SDK uses the same 2-element shape (`buzz:crates/buzz-sdk/src/builders.rs:1917-1928`).
- `expiration` (NIP-40, unix seconds string) is present only when text or emoji is non-empty and a duration is chosen.
- **Clear** = publish with empty content and no emoji tag (tags `[["d","general"]]`).
- Limits: text ≤100 and emoji ≤100 UTF-16 units, no `\r`/`\n` (`STATUS_TEXT_LIMIT = 100`, `user-status-policy.ts:1-11`).
- `created_at = max(now, previousOwn.created_at + 1)`. The save fails if that is more than now + 300 s (clock skew). Before signing, the client does a fresh read of its own current status: `{kinds:[30315], "#d":["general"], authors:[viewer], limit:1}`. Signed output is checked against the template. 12 s timeout. One save at a time (`user-status.ts:208-284`).

### 5j.2 Durations (`src/features/user-status/status-duration.ts:1-38`)

Presets: `3600` (1 hour), `28800` (8 hours), `today` (local midnight tonight), `week` (next local Monday 00:00), `custom`, `never` (no expiration tag). The preset is not stored. When editing, the client infers the preset whose deadline is within 120 s of the saved expiry.

### 5j.3 Fetch, live, caching (`user-status.ts:45-195`)

- Watch-based: `watch(ids)` is ref-counted. Missing ids are batch-read in chunks of **100**: `{kinds:[30315], "#d":["general"], authors, limit:authors.length}` at background priority.
- On failure: retries at 5 s, 15 s, 60 s, then stops.
- Live: global `{kinds:[30315], "#d":["general"], since, limit:500}` on the profiles route (`live.ts:619-625`). Live events for unwatched people (other than the viewer) are ignored.
- Replacement: newer `created_at` wins. On equal `created_at`, the **lower** id wins: an incoming event is skipped when `old.id <= event.id`, which matches `newer()` (`src/features/relay/user-status.ts:108-115`).
- Snapshot excludes empty or expired statuses. A timer re-publishes at the earliest `expiresAt`. Unwatching evicts the whole coordinate (except the viewer's own). `current(userId)` does a fresh single read.

---

## 5k. Custom emoji (kind 30030)

### 5k.1 Model

- Each member publishes **one** user-signed kind 30030 with `d = "buzz:custom-emoji"`. The **community palette is the client-side union of every member's set** (`src/features/relay/emoji.ts:4-5`; `buzz:crates/buzz-sdk/src/builders.rs:527-563`).
- **No kind 10030 user emoji list is used.** `KIND_EMOJI_LIST = 10030` exists in core (`buzz:crates/buzz-core/src/kind.rs:34`), but the app never references it, and there are no per-user subscriptions to other sets.

```json
{
  "kind": 30030,
  "content": "",
  "tags": [
    ["d", "buzz:custom-emoji"],
    ["emoji", "partyparrot", "https://relay.example/media/77aa…png"],
    ["emoji", "shipit", "https://relay.example/media/88bb…gif"]
  ]
}
```
Validity (`emoji.ts:32-67`; broker admission): `content === ""`. `tags[0]` is exactly `["d","buzz:custom-emoji"]`. Every other tag is exactly `["emoji", code, url]`. `code` is already normalized (`[a-z0-9_-]{1,64}`, lowercase) and unique. `url` is non-empty, has no surrounding whitespace, and is ≤2048 chars. SDK additionally requires `http(s)://` (`buzz:crates/buzz-sdk/src/builders.rs:157-176`). `created_at` ≤ now + 300.

Shortcode normalization (`emoji.ts:11-18`): trim, strip leading/trailing `:`, lowercase, must match `^[a-z0-9_-]{1,64}$`. `suggestShortcode(filename)` drops the extension, maps invalid runs to `_`, and trims `_`/`-` (`emoji.ts:19-30`).

### 5k.2 Discovery and caching (`src/features/relay/emoji-directory.ts`)

- `ensure()` performs one read, `{kinds:[30030], "#d":["buzz:custom-emoji"], limit:500}`, at background priority (`emoji-directory.ts:111-150`). If **≥500** events are returned, the status is `error` ("catalog may be incomplete").
- Budget: ≤500 member sets and ≤**2 MiB** total (`LIMIT`, `MAX_BYTES`, `emoji-directory.ts:29-30`). Overflow sets `limited` and the error status, and clears entries until an explicit `refresh()`. **No LRU eviction**, because dropping a replacement could resurrect a removed code.
- Per author, the newest set wins (`newer`). Merge rule across authors: for each shortcode, the set with the **newest `created_at`** wins. On a tie, the lexicographically smaller URL wins. Entries are sorted by shortcode (`emoji-directory.ts:89-105`). `mine` = the viewer's own set entries.
- Live updates: the profiles route subscribes `{kinds:[30030], "#d":["buzz:custom-emoji"], since, limit:500}` (`live.ts:626-631`). On reconnect it re-reads, and after `clear()` it re-ensures once.

### 5k.3 Adding / replacing (read-modify-write)

`add(name, url)` (`emoji-directory.ts:155-227`, `:242-258`) is serialized locally:
1. Fresh read of own set: `{kinds:[30030], "#d":["buzz:custom-emoji"], authors:[viewer], limit:1}` (12 s timeout).
2. New tags: `["d", …]`, then existing entries minus the same shortcode, then the new `["emoji", code, url]` appended last.
3. `created_at = max(now, current.created_at + 1)`. Sign, verify the signed output matches the template, publish, then accept locally.

Upload: image must be `image/*` (picker accepts gif/png/jpeg/webp). It goes through the normal attachment prepare and upload path, and the result's `url` (relay media) is used (`src/bundled/emoji/CustomEmojiSettings.tsx:127-165`). **There is no remove-emoji UI in the app**. The SDK documents removal as read, mutate, rebuild (`buzz:crates/buzz-sdk/src/builders.rs:533-539`).

### 5k.4 Referencing emoji from messages and reactions

- The composer completes `:query` with 2–64 chars from `[a-zA-Z0-9_+-]` after start, whitespace or `([{` (`src/bundled/emoji/emoji-query.ts:2-9`). It inserts `:code:` text.
- On send, `emoji.tags(content)` (`emoji-directory.ts:292-311`) finds `:code:` tokens outside `http(s)` URLs (`emoji.ts:82-112`). For each code present in the **community catalog** it emits `["emoji", code, url]`. Unknown codes stay plain text with no tag. **If the catalog is not `ready` and the content references any `:code:`, send throws** ("Community emoji is loading…" / "unavailable") and the draft is kept.
- Messages and edits render custom emoji **from the event's own `emoji` tags only**, not from the live catalog. For an edited message, tags come from the latest edit if it has any (`fold.ts:416-421`; `src/bundled/emoji/index.tsx:17-22`, `:66-75`). Matching is case-insensitive on the code, and the first tag per code wins (`emoji.ts:69-80`).
- Reactions: see 5d.9 (`content ":code:"` plus exactly one `["emoji", code, url]`).
- Image display uses `session.media(url)` (authenticated media proxy, see media section). URLs in events stay original.
- Unicode picker: emoji-mart with bundled `@emoji-mart/data`. The custom category prefix is `buzz-custom/` (`src/bundled/emoji/emoji-mart.ts:5-15`). `reactionName` maps native emoji to `:name:` for labels (`src/features/messages/reaction-name.ts`).

---

## 5l. Notifications and inbox

### 5l.1 Desktop notification triggers

Source: `src/features/notifications/messages.ts:46-225`, `src/features/relay/session.ts:2322-2444`, `docs/notifications.md:45-93`.

Candidate source = `session.subscribeIncoming`, only for the **selected community's** live session (no background communities). Session emits an `IncomingMessage` iff all:
- live phase (not history/replay/initial or reconnect catch-up), route has a single `channelId`;
- kind ∈ {9, 40002, 40008} (doc says 9/40002; code also includes 40008);
- `pubkey != viewer`; exactly one `h` tag equal to the route channel;
- not already in the session's recent cache, not already retained unread evidence, not a local outbox event;
- passes visibility/access.

```ts
IncomingMessage = { channelId, messageId, createdAt, authorId, workflowOwnerId?, previewContent /* ≤4096 chars; 40002 uses body.content */ }
```

Admission per message (`messages.ts:90-187`):
```text
age = now_ms - createdAt*1000; skip if age < -30000 or age > 120000
a = unread.attention(channelId, messageId)
category = a.category ?? (a.pending ? "thread" : none)
skip unless category && (a.status=="eligible" || a.pending)
notifications.admit(category, label, {sourceKey: messageId, target: conversation(...)}, valid, eligible, text)

eligible():   # re-evaluated until delivered; returns true | false | "wait"
  if !valid() || now - createdAt*1000 > 120000: false
  a = attention(...); sync = unread.sync()
  if !a.mentioned:
     if sidebarPreferences.muted ∋ channelId: false          # mute suppresses DM + thread, NOT mentions
     if prefs.status not in {ready, unsupported}: "wait"
  if a.status=="ineligible" || (!a.pending && a.category != category) || (!notifyWhileViewing && a.viewing): false
  if sync.status in {loading,error} || (sync.capability!="unsupported" && sync.completeness=="unknown") || a.status=="unknown": "wait"
  return a.unread || (notifyWhileViewing && a.viewing)
```
Categories: `mention` ("Mentions"), `direct` ("Direct messages"), `thread` ("Thread replies"). Category = `direct` in DM channels (even if p-tagged), else `mention` if `mentionsViewer`, else `thread` if `conversation(entry)`. **Broadcasts** (`["broadcast","1"]`) do not notify. **No keyword notifications** (not found). `attention.viewing` = message id is in a live `view()` lease of a visible reading surface (`unread.ts:675-677`).

Target sent with the candidate:
```json
{"version":1,"kind":"conversation","scope":{"viewer":"<hex>","communityOrigin":"<url>"},
 "channelId":"<c>","messageId":"<id>","threadRootId":"<root?>"}
```

### 5l.2 Notification service policy

Source: `src/features/notifications/service.ts:56-480`, `preferences.ts:9-163`.
- Preferences: localStorage `buzz-notification-preferences.v1:<viewerHex|device>`:
  ```json
  {"enabled":true,"notifyWhileViewing":false,"sound":true,
   "sounds":{"mention":"<SoundName>","direct":"...","thread":"..."},
   "categories":{"mention":true,"direct":true,"thread":true}}
  ```
  Unreadable stored prefs → `enabled:false` + error (fail closed). Category keys `^[a-z0-9][a-z0-9._/-]{0,255}$`, ≤128.
- `allowed(item)`: not closed, not dev-paused, not cancelled, same viewer, `expires > now` (`FRESH_MS=120_000`), `valid()`, target authorized, `enabled`, `categories[cat] !== false`.
- Dedup key `JSON.stringify([communityOrigin|"local", "message"|pluginCategory, sourceKey])`, kept 120 s, map ≤2048 entries. Pending ≤128 (else error "Too many pending notifications").
- Delivery: after UI presentation settles (`afterPresentation`), re-check `allowed` + `eligible()`; `"wait"` stays pending (re-checked on `revalidate()`, triggered by unread sync changes, membership lookups finishing, preference/roster changes). Permission must be `granted` or `unknown` (Linux/Windows report unknown). Exactly one OS submission attempt; errors reported, never retried.
- Banners are always silent at OS level; app plays the per-category sound after acceptance unless Sound off / category `silent` / revoked in the meantime.
- Text (`content.ts:69-92`): title `"<sender> mentioned you in #<channel>"` / `"<sender> sent you a direct message"` / `"<sender> replied in #<channel>"` (DM destination text = "a direct message"); sender = cached profile/name ≤64 cp, else first 10 hex of pubkey; workflow output uses workflow label of owner. Body = first 4096 chars of content → CommonMark flattened (HTML/definitions dropped, images `[Image]`) → ≤200 code points with `…`; empty → "New message".
- Click → `navigation.open(target)` (no mark-read).

### 5l.3 Host side (Tauri) data

- `notification_show(id: uuid, title ≤4096 B, body ≤4096 B)`; ≤128 active callbacks (`MAX_ACTIVE`) (`src-tauri/src/notifications.rs:7,125-165`).
- Linux: zbus `org.freedesktop.Notifications.Notify("Buzz", 0, <exe name icon>, title, body, ["default","Open"], {"suppress-sound": true}, -1)`; requires `actions` capability (`src-tauri/src/notifications/linux.rs:23-60`).
- Dock badge: `unread_indicator_set(unread: bool)` → `window.set_badge_label(unread ? "•" : None)`, main window only, macOS only; other OS returns error and the frontend never binds it (`src-tauri/src/dock.rs:21-35`, `indicator.ts:14-21`). Permission: `dock_permission(request)` → `default|setup|enabled|disabled|denied|unavailable`.

### 5l.4 Dock badge / app unread indicator

Source: `src/features/notifications/indicator-unread.ts:5-96`, `indicator.ts:24-128`.
```text
for selected community session (viewer matches, membership present):
  subscribe unread.snapshot({kind:"channel", channelId}) for every listed channel with viewer ∈ members
  unread = any(snapshot.manual != "none" || (snapshot.observedCount ?? 0) > 0)
project dot iff unread && permission == "enabled"; single ordered writer, no retry loop
```
Boolean dot only, no count. Includes muted channels and thread replies (no mute filter found). Cleared on account/community change, access loss, dispose.

### 5l.5 Inbox projection (`unread.inbox()`)

Source: `unread.ts:904-1046`, `inbox.ts:1-30`, `docs/inbox.md:116-183`.
Built purely from retained unread evidence; same `category()` rules.
```text
for channel in roster where !cached && !archived && viewer ∈ members:
  dm = channelType=="dm"
  groups: for each evidence e with pubkey != viewer and category(e,dm) != none:
     groupId = dm ? channel.id : (e.rootId ?? e.event.id)
  for each group (entries sorted created_at asc, id asc):
     latest = last; unread = entries filtered isUnread; representative = unread[0] ?? latest
     lastReply = last entry with rootId
     target = dm ? {channel} : (lastReply.rootId retained & not deleted ? {thread, rootId} : {message, latest.id})
     readThrough = dm ? [] : [{message target} for entries that are top-level or have localUnread msg:]
                         + (lastReply ? [{thread target, messageId: lastReply.id}] : [])
     item = {id:"<channel>:<groupId>", channelId, target, messageId: representative.id, latestMessageId,
             messageIds, rootId?, authorId, workflowOwnerId?, preview (folded), createdAt: latest.created_at,
             mentioned: any, thread: any has parentId, unreadCount: |unread|,
             manual: localUnread[targetKey(target)] || any readThrough key in localUnread, readThrough}
items sorted createdAt desc, id asc
status = error ? "error" : repairing ? "loading" : freshness=="unknown" ? "idle" : "ready"
```
Archived channels are excluded but their evidence/read state retained. Opening Inbox does not mark read; selecting a row runs its `readThrough` steps (non-atomic; DM → channel read with captured cutoff). UI filters: activity type, sender, "Unread only" (`hasUnread = unreadCount>0 || manual`, `src/bundled/inbox/InboxPage.tsx:67`).

### 5l.6 Inbox feed (addressed history demand)

Source: `src/features/relay/inbox-feed.ts:1-253`, `session.ts:863-890`.
No `feed_types`. On `ensure()` (when idle) / `refresh()` / reconnect (if previously requested):
```text
1. addressedRead({kinds:[9,40002], "#p":[viewer], limit:50})   # verified; admitted into unread evidence
   prepare(): before unread subscribers are notified, mark projected ids "incomplete"
   project = kinds 9/40002, p-tags viewer, and (no h tag || single h tag of a listed member channel)
2. targets = incomplete ∪ projected ids
   overlays(targets, [40003,5,9005]): page {kinds, "#e":targets, limit:500, until, before_id} until empty page;
     cursor must strictly advance; ≤2000 events and ≤4 MiB total, else error
3. overlays(retainedEditIds(targets) ∪ fetched edit ids, [5,9005])
4. status ready, incomplete=[]
```
Session wrapper fails the read if any raw kind 40003/5/9005 event was withheld by visibility (`session.ts:866-881`). `incomplete` ids must not show bodies as current. Inbox feed has no live route of its own; live arrivals reach the inbox through ordinary unread evidence.

### 5l.7 Not present

- No server-side unread counts or notification feed used by the client; no push for closed app; no notification history DB; no keyword alerts; no cross-community background notifications; no numeric dock/taskbar badge; no Windows/Linux tray/badge (`docs/notifications.md:95-99,186-189`).

---

## 5m. Channel lifecycle and metadata

### 5m.1 Create (9007)

Template (`src/features/relay/work-sessions.ts:500-541`), via durable outbox (adds `client-id`):
```json
{"kind":9007,"content":"",
 "tags":[["h","<crypto.randomUUID()>"],["name","<canonical>"],["visibility","open"|"private"],
         ["channel_type","stream"],["about","<desc>"]?,["ttl","<secs>"]?,["client-id","<uuid>"]]}
```
Exact order enforced natively (`channel_writes.rs:336-401`): `h,name,visibility,channel_type`
then optional `about`, `ttl`, `client-id`; `channel_type` must be `stream` (no forum/dm creation);
name non-blank ≤120 chars; about ≤1000 and must not contain `"Buzz session ("` unless exact session
metadata; ttl positive ≤ 2147483647, no leading 0. Canonical name = strip leading `#`/whitespace,
trailing whitespace (`channel-details-protocol.ts:40-42`, `buzz:crates/buzz-core/src/channel.rs:15-18`).
Default temporary TTL constant: `7*24*60*60` (`work-sessions.ts:7`). Session channels:
`visibility=private`, `about = "Buzz session (buzz.sessions/v1)"[ + "\nparent:<uuid>"]`.

**Group id is client-generated** (`h` UUID). Relay pre-creates channel with that UUID at ingest;
reused UUID => `accepted:false, message:"duplicate: channel already exists"`
(`buzz:crates/buzz-relay/src/handlers/ingest.rs:3061-3097`). Creator becomes `owner`. Side effects: 40099
`channel_created`, 39000/39001/39002, 44100 to creator (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1906-2032`).

Confirmation (`session.ts:1481-1572`, `work-sessions.ts:40-140`): wait outbox delivery; on
failed/unknown, check exact creation receipt `{"kinds":[9007],"ids":[id],"authors":[viewer],
"limit":1,"consistency":"strong"}` or `{kinds:[39000,39002],"#d":[ch],limit:2,strong}` + `{ids:[id]}`;
then `refresh(id, {member: viewer})` (roster contains viewer). One pending creation at a time;
persisted in outbox and restored on restart (same UUID; retry republishes exact same event).
Template setup path (`createChannelSetup`) runs create then group assignment (sidebar `assign`).

### 5m.2 Join (9021) / Leave (9022) / Delete (9008) / Archive / Unarchive / Hide

Templates (`channel-lifecycle-protocol.ts:23-73`), content `""`, `created_at` now:

| Action | Kind | Tags |
|---|---|---|
| archive | 9002 | `[["h",id],["archived","true"]]` |
| unarchive | 9002 | `[["h",id],["archived","false"]]` |
| delete | 9008 | `[["h",id]]` |
| join | 9021 | `[["h",id]]` |
| leave | 9022 | `[["h",id]]` |
| hide (DM) | 41012 | `[["h",id]]` |

Permissions from fresh 39000/39001/39002 (`channel-lifecycle-protocol.ts:110-180`):
`canArchive` = type≠dm && !archived && owner|admin; `canUnarchive` = type≠dm && archived &&
owner|admin; `canDelete` = type≠dm && !archived && owner; `canLeave` = type≠dm && !lastOwner
(`leaveReason: "Transfer ownership before leaving the channel."`); `canHide` = type==dm. Viewer must
be in roster. Owner-agent evidence: if not granted directly, read kind-0 of 39001 owners in batches of
4 (5 s budget); a verified NIP-OA owner == viewer grants archive/unarchive/delete
(`channel-lifecycle.ts:133-218`). Server mirrors (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:609-683,746-772`): 9002 privileged
tags need owner/admin or owner-of-owner-agent; 9008 owner or owner-of-owner-agent; 9022 not sole owner;
9021 only `visibility=open` (else "channel is private — request an invitation"), no-op if member.

Join preconditions (`channel-lifecycle.ts:302-320`): fresh 39000 must be `openMetadata` and not
archived; if viewer already in 39002 => accept discovery, no publish.

Run flow (`channel-lifecycle.ts:286-433`): authorize -> sign -> verify signed fields unchanged ->
authorize again -> publish (not outbox; native `relay_channel_sign` / `relay_channel_publish` with
route `channel-lifecycle`, `channel_writes.rs:188-221`) -> confirm by polling at `[0,250,750,1500]` ms
(20 s overall):
- hide: 30622 contains id;
- join: strong `39002 #d=id #p=viewer` contains viewer -> `acceptDiscovery`;
- leave: strong `39002 #d #p=viewer` **absent** -> `removed(id)` (access-loss purge);
- delete: strong `39000 #d` **absent** -> `removed(id)` (server soft-deletes discovery events, `buzz:crates/buzz-relay/src/handlers/side_effects.rs:2034-2085`);
- archive: 39000 `archived=="true"`; unarchive: 39000 present with `archived` absent or `"false"`.
Post-publish failure (non-`PublishRejected`) => `ChannelLifecycleUnconfirmed`; no auto retry.
Only one lifecycle action at a time.

Archive effects: server rejects all admin kinds on archived channels except unarchive; unarchive
re-sends 44100 to every member (resubscribe trigger) (`buzz:crates/buzz-relay/src/handlers/side_effects.rs:1729-1785`). Client hides
archived from sidebar; still reachable via search.

### 5m.3 Edit metadata (9002 details)

`channel-details-protocol.ts:136-247`, `channel-details.ts:35-234`. Full edit:
```json
{"kind":9002,"content":"",
 "tags":[["h",id],["name",name],["about",desc],["visibility","private"|"open"]?,["ttl","<secs>"|""]?]}
```
`visibility` only when changed (note: write value `open`, read back as `public` tag); `ttl` only when
changed, `""` clears (permanent). Session rename: exactly `[["h",id],["name",name]]`. Name: 1–120
chars, already canonical; about ≤1000, no `"Buzz session ("`. `canEdit` = type≠dm && not archived &&
not child session && viewer owner|admin. Optimistic concurrency: `details.version = 39000.id`; save
re-reads and rejects if version changed ("Channel details changed…"). Confirm via strong re-read that
name/about/visibility/ttl equal draft; on uncertainty keep `attempts[id] = {draft, status:"unconfirmed"}`
and require explicit `check()` before another save. Server also accepts `topic`/`purpose` tags
(any member) — not used by the app.

### 5m.4 Templates

Create-from-template is `createChannelSetup` (`session.ts:1309-1450`): 9007 then kit writes (Canvas,
members, group assignment). Kit/template payloads are outside this section; see 5a.8 for d-tags.

### 5m.5 Not found / not supported

- No `picture` channel metadata in relay output or app writes.
- No forum or DM creation via 9007 in the app (native validator rejects non-`stream`).
- No 9009 invites, 39003 roles usage in app.
- No debounce on sidebar preference writes (serialized queue + strong readback instead).

---

## 5n. Media upload and download

### 5n.1 Client-side limits and preparation
Limits (`src/features/relay/attachment-limits.ts`):
- `UPLOAD_MAX_BYTES` = 500 MiB; `UPLOAD_TIMEOUT_MS` = 600,000.
- Per-type cap `mediaByteLimit`: GIF 10 MiB, other `image/*` 50 MiB, `video/*` 500 MiB, everything else 100 MiB.
- The relay may enforce lower limits. Server defaults: video 500 MB, file 100 MB, gif 10 MB (`buzz:crates/buzz-media/src/config.rs:36-79`).

Draft limits (`src/features/messages/attachment-draft.ts:34-35,257-266`):
- At most **10 files per message**; at most 1000 MiB of retained bytes per draft.
- Uploads run **one at a time** in `prepareForSend`.

Image sanitizing (`src/features/messages/prepare-attachment.ts`, `image-metadata.ts`), done in JS before upload:
- Sniff the image type from magic bytes (`imageType`).
- GIF: run `cleanGif` (strip metadata).
- PNG: animated PNGs are cleaned chunk-wise. Static PNGs are re-encoded.
- WebP: animated, or no pixel transform needed → `cleanWebp`. Otherwise re-encode to lossless WebP.
- JPEG: re-encode via canvas at quality 0.95, applying EXIF orientation.
- At most 25 M pixels.

A Rust port must strip EXIF/ICC/text metadata equivalently. The server rejects some metadata: a 400/415/422 whose text matches `/metadata/` maps to the `"metadata"` error.

HEIC and video (`src/features/relay/video-preparation.ts`, `native.ts:224-279`, `src-tauri/src/relay/media_preparation.rs`):
- JS sniffs a fixed demuxer from the first 4 KiB and sends mode `x-buzz-preparation: image:mov | video:{mov,avi,matroska,flv,asf,mpeg,m4v,mpegvideo} | voice:{wav,flac,ogg,aiff,mp3,aac,amr,mov}`.
- Rust runs the system **ffmpeg** with fixed arguments:
  - Image → JPEG: `-map_metadata -1 -frames:v 1 -q:v 2`. Needs ffmpeg ≥ 8 for HEIC grids.
  - Video → MP4: `libx264 -preset fast -crf 23 -pix_fmt yuv420p`, pad to even dimensions, `aac 128k`, `+faststart`, metadata stripped.
  - Voice notes (filename `voice-note-*.wav`, ≤ 128 MiB) → MP4 with a 16x16 black video track (`lavfi color`) and `-shortest`.
- Deadline 600 s. Caps: input 500 MiB, image output 50 MiB, video output 500 MiB.
- Error JSON `{"code":"size"|"capacity"|"ffmpeg"|"image"|"video"|"io"}` with status 413/429/503/400 (`media_preparation.rs:16-49`).
- Resulting type is `image/jpeg` (name `.jpg`) or `video/mp4` (name `.mp4`).

Voice recording limits: WAV only, ≤ 300 s, ≤ `44 + 300*24000*2` bytes, waveform of 1-100 values (`src/features/relay/voice-media.ts`).

### 5n.2 Native upload pipeline (IPC → spool → PUT)
JS `nativeUpload` (`native.ts:139-220`):
1. `relay_upload_begin {id: uuid, size}`.
2. Repeatedly `relay_upload_chunk`: raw-bytes body ≤ 64 KiB, headers `x-buzz-upload-id` and `x-buzz-upload-offset`. Chunks must be sequential.
3. `relay_upload`: empty body, headers `x-buzz-upload-id`, `x-buzz-community: <origin>`, `x-buzz-content-type`, optional `x-buzz-preparation`, optional `x-buzz-upload-progress: <Channel id>` (progress messages `{sent,total}`, sent at most once per percent).
4. Cancel with `relay_upload_cancel {id}`. A cancel that arrives before the upload starts is remembered for 60 s.

Rust side:
- Upload IDs match `[A-Za-z0-9-]{1,64}`. At most 64 active uploads, else "Uploads are busy" (`relay.rs:859-936`).
- Spools are private temp dirs, at most **4** concurrent; idle ones expire after 60 s. SHA-256 is computed incrementally during `append` (`src-tauri/src/relay/upload_spool.rs:14-15,149-339`).
- `process_spool` (`relay.rs:1028-1078`) prepares the file if a mode is set, then re-hashes it.

`upload_file` (`relay.rs:1145-1189`) sends:
```
PUT https://relay.example.com/upload
Authorization: Nostr <24242 t=upload, x=<sha256>, server, expiration>
Content-Type: <x-buzz-content-type if a valid MIME token, else application/octet-stream>
X-SHA-256: <sha256 hex>
Content-Length: <size>
<streamed body, 64 KiB chunks>
```
- Timeout 600 s. Size must be 1..=500 MiB (`relay.rs:818,1092-1097`).
- The response is read with an **8192-byte cap**.

Server routes (`buzz:crates/buzz-relay/src/router.rs:286-294`, `buzz:crates/buzz-relay/src/api/media.rs:300-420`):
- `PUT /upload` (`PUT /media/upload` is a legacy alias); `GET`/`HEAD /media/{sha256}.{ext}`.
- Upload checks: Blossom auth, then `X-SHA-256` (64 lowercase hex, must equal the auth `x` tag), the **membership gate**, a per-pubkey rate limit and a concurrency permit.
- The server sniffs content type from the first 4 KiB and ignores the client's `Content-Type`.

### 5n.3 Response descriptor (Blossom BUD-02)
`buzz:crates/buzz-media/src/types.rs:7-31`:
```json
{"url":"https://relay.example.com/media/<sha256>.<ext>","sha256":"<64hex>","size":123,
 "type":"image/jpeg","uploaded":1760000000,"dim":"1920x1080","blurhash":"...","thumb":"https://.../media/<sha256>.thumb.jpg","duration":12.5}
```
- `ext` comes from `mime_to_ext`: jpg/png/gif/webp/mp4, else `bin` (`buzz:crates/buzz-media/src/validation.rs:946-955`).

Client error mapping (`readUploadResponse`, `src/features/relay/attachments.ts:220-281`):
- 401/403 → `denied`; 413 → `size`; 429 → `capacity`.
- Body larger than 8192 bytes → `invalid`.
- Non-OK with JSON `code` ∈ `UPLOAD_FAILURES` → that code. 400/415/422 → `metadata` or `rejected`. Anything else → `failed`.

Validation (`validateUploadResult`, `attachments.ts:55-97`):
- `type` must match `/^[a-z0-9.+-]+\/[a-z0-9.+-]+$/i`.
- `size` must equal the bytes sent and be within `mediaByteLimit(type)`.
- `sha256` must be 64 lowercase hex.
- `url`, resolved against the origin, must be same-origin with no userinfo, query or hash, and its path must match `^/media/<sha256>(\.[a-z0-9]{1,8})?$`.
- The client keeps only `{name,url,type(lowercased),size,sha256}`. **`dim`, `blurhash`, `thumb` and `duration` are dropped.**

### 5n.4 imeta tag and message content
`attachmentMessage` (`attachments.ts:284-334`) builds one tag per file, in this exact field order:
```json
["imeta","url https://relay.example.com/media/<sha>.jpg","m image/jpeg","size 12345","x <sha>","filename photo.jpg"]
```
- Voice files append `"duration <secs>"` and `"waveform <n n n ...>"` (integers 0-100, at most 100 values).
- `filename`: control chars, `/` and `\` become spaces; truncated to 255 UTF-8 bytes; trimmed; `"File"` if empty.
- The client **never** emits `dim`, `blurhash`, `thumb` or `alt`.

Message content is `[text.trim(), ...links].filter(Boolean).join("\n\n")`, where each link is:
- `![label](<url>)` if the type is `image/(png|jpeg|gif|webp)` or `video/mp4`;
- `[label](<url>)` otherwise.
- The label escapes ``\`*_{}[]()!<>&``.

Server imeta rules (`buzz:crates/buzz-relay/src/handlers/imeta.rs:10-200`, applied on every ingested event that has imeta, `handlers/ingest.rs:3185-3197`):
- Allowed keys: `url m x size dim blurhash alt thumb fallback duration bitrate image filename`. All except `fallback` are singletons.
- `url`, `m`, `x` and `size` are required.
- `url` must be a local `/media/` URL and must not be a thumbnail.
- The hash in `url` must equal `x`. For media MIME types the extension must match `m`.
- `duration`, `bitrate` and `image` are allowed only with `video/mp4`.
- `verify_imeta_blobs` cross-checks the stored blob sidecar: hash, size and MIME.

**DISCREPANCY:** `waveform` is not in the server's allowed keys in this snapshot, so a voice note with `waveform` would be rejected with `"invalid: disallowed imeta key: waveform"`. Confirm against the deployed relay before porting.

Reading imeta (`src/features/relay/fold.ts:84-162`):
- Parses `dim` (`^\d{1,6}x\d{1,6}$`), `blurhash`, `image`/`thumb` as the preview URL, `size`, `duration`, `filename` and `waveform`.
- Blurhash syntax check (`src/features/relay/blurhash.ts`): base83, 6-166 chars, `len == 4 + 2*components`.
- Markdown images without imeta are also treated as attachments.

### 5n.5 GIFs (no upload)
- Search: `POST /gifs/search`, NIP-98 signed, body `{"customer_id":"<uuid>","locale":"en-US","query":"..."}`. Only available if NIP-11 advertises it (§1.4).
- The response is KLIPY-shaped (`data.data[]` with `file.{hd,md,sm,xs}.{gif,webp}.{url,width,height,size}`).
- Selection inserts markdown `![title](<klipy https url>)`, with `(`/`)` percent-encoded. No imeta, no upload.
- Source: `src/features/relay/gifs.ts:168-237`.

### 5n.6 Download / display (auth required)
- `GET /media/<sha>.<ext>` **requires** Blossom `t=get` auth plus relay membership (`buzz:crates/buzz-relay/src/api/media.rs:790-835`). The thumbnail is `/media/<sha>.thumb.jpg`.
- `mediaUrl` (`src/features/relay/transport.ts:178-192`):
  - URLs under `${origin}/media/` go through the native proxy. `size==="small"` (avatars) rewrites to `.thumb.jpg`.
  - Other `https://` URLs load directly, unauthenticated.
  - `data:` URLs are allowed only for avatars.
- The native proxy has two transports:
  1. **`buzz-media://localhost/<percent-encoded relay URL>`** URI scheme for `<img>` (`relay.rs:1206-1235`, `media_request` `1622-1661`):
     - GET only. The URL must be an HTTPS origin, path `/media/<64 hex>[.ext]` with ext `[a-z0-9]{1,8}` or `thumb.jpg`, no query or fragment.
     - A fresh `get` token is signed per request. Only one `Range: bytes=S-[E]` is forwarded, clamped to 4 MiB.
     - Whole responses are capped at 100 MiB (`MAX_MEDIA`). Ranged reads use a 1 MiB block cache of 32 blocks (`src-tauri/src/relay/media_blocks.rs`).
     - Origins that ignore `Range` for video are spooled to a temp file, capped at 500 MiB, at most 2 spools, 60 s idle (`src-tauri/src/relay/media_spool.rs`).
     - Timeout 600 s (`relay.rs:1693-1717`).
     - Response headers: upstream `Content-Type` if it is image (not SVG), video or audio; otherwise `application/octet-stream` plus `Content-Disposition: attachment`. Always `Accept-Ranges: bytes`, `Cache-Control: private, max-age=3600`, `X-Content-Type-Options: nosniff` (`relay.rs:1729-1740,1790-1809`).
  2. **Loopback HTTP** for `<video>`/`<audio>` (WebKitGTK cannot play custom schemes) (`src-tauri/src/relay/media_stream.rs:1-110`):
     - URL form `http://127.0.0.1:<port>/<256-bit token>/<percent-encoded relay URL>`. The base comes from command `media_stream_base` (`string | null`).
     - Checks: `Host` must equal the bound address; `Origin`, if present, must be the app.
     - One request per connection, at most 32 connections, 5 s to receive the request head (8 KiB max).
- Host-owned media commands: `media_download {source, name}` (saves to the OS Downloads folder without overwriting), `media_copy_image {source}`, and `media_snapshot_read {source, max_bytes ≤ 16 MiB}` (`relay.rs:1552-1620`).
- Avatar prefetch: at most 2 concurrent decodes, 10 s timeout, queue ≤ 24 (prepare) or 48 (warm). Skipped when `saveData` is on (`src/features/relay/media.ts:37-143`).
- There is no persistent media cache in Rust beyond the block cache and spools. The webview HTTP cache honors `max-age=3600`.

---

## 5o. Other kinds: reminders, GIFs, work sessions

**Reminders (kind 30300, NIP-ER).** Self-encrypted (NIP-44 v2 to self), author-only parameterized-replaceable records (`src/features/relay/reminders.ts:1-5`; `buzz:crates/buzz-core/src/kind.rs:96-102`).
- `d` = random 32 hex chars (16 bytes) (`reminders.ts:122-126`).
- Tags are exactly `[["d", d], ["not_before", "<unix>"]]` for a pending reminder, or `[["d", d], ["expiration", "<unix>"]]` for done/cancelled. Expiration is 30–90 days out, randomized (`reminders.ts:130`; signer `src-tauri/src/relay/reminders.rs:174-215`, which accepts 29–91 days).
- Encrypted plaintext: `{"status":"pending|done|cancelled","target":{"eventId","channelId","preview","authorPubkey"}?,"note"?}`. At least one of target or note is required.
- Read: `{kinds:[30300], authors:[viewer], limit:200}`. Live: `{kinds:[30300], authors:[viewer], since, limit:500}` on the membership route (`live.ts:610-615`).
- Operations: create, snooze (new `not_before`), complete, and cancel. Each replaces the same `d`. Signing and encryption live in the Rust host (`relay_sign_reminder`).

**GIFs (no event kind).** Discovery is via the NIP-11 relay info document. It requires `supported_extensions` to include `"buzz-gif"`, `gif.provider == "klipy"` and `gif.search` to be a safe absolute path. The relay advertises `/gifs/search` and `/gifs/share` (`src/features/relay/gif-capability.ts:10-22`; `buzz:crates/buzz-relay/src/nip11.rs:253`, `:652-654`).
- Search: `POST /gifs/search` (NIP-98-protected, `buzz:crates/buzz-relay/src/router.rs:330`, `:3278`) with body `{"customer_id": <uuid persisted in localStorage "buzz:klipy-customer-id:v1">, "locale": navigator.language||"en-US", "query": "<trimmed>"}`.
- Response: KLIPY JSON `{result, data:{data:[{id,slug,title,type:"gif",file:{hd|md|sm|xs:{gif|webp:{url,width,height,size}}}}]}}`. The original image is taken from md→hd→sm→xs gif and the preview from sm/xs webp/gif. Assets must be https with positive dimensions (`src/features/relay/gifs.ts:84-120`, `:195-228`).
- Sending a GIF inserts markdown `![title](url)` into a normal kind 9, with `(`/`)` percent-encoded. There is no imeta and the media is not re-uploaded (`gifs.ts:229-237`).
- `/gifs/share` is not called by the app.
- Native packaged builds cache the discovered path per community and forget it on failure (`gifs.ts:157-180`).

**Work sessions (no new kinds).** These are NIP-29 group commands through the durable outbox (`src/features/relay/work-sessions.ts`).
- Channel/session create is kind **9007** with tags `["h", uuid]`, `["name", ≤120 chars]`, `["visibility", "open"|"private"]`, `["channel_type", "stream"]`, optional `["about", ≤1000]` and optional `["ttl", seconds]`. Default temporary TTL is 7 d = 604800 (`work-sessions.ts:7`, `:504-535`).
- Sessions are private streams with `about = "Buzz session (buzz.sessions/v1)"`, or that value plus `"\nparent:<parentUuid>"` (`src/features/sessions/metadata.ts:1-8`; `work-sessions.ts:537-556`).
- Adding a member or agent is kind **9000** `[["h", uuid], ["p", pubkey]]` (`work-sessions.ts:463-473`, `:572-583`). A stale add request older than 15 min must be dismissed (`work-sessions.ts:455-462`).
- Confirmation reads use `{ids:[id], limit:1, consistency:"strong"}` and `{kinds:[39000,39002], "#d":[uuid], limit:2}`. Membership discovery uses `{kinds:[39002], "#p":[pk], limit:500}` (`work-sessions.ts:60-120`, `:322-339`).

---

## 6. Outbox and durability

There is one outbox per relay session, keyed by partition `origin:viewer`. It owns three things: pending delivery, a bounded journal of confirmed events, and one change stream that every view reads. The core is `src/features/relay/outbox.ts` (915 lines). IndexedDB storage is in `outbox-storage.ts`. The wiring is in `session.ts:264-317`.

### 6.1 Record shape (persisted verbatim as `operation`)

`src/features/relay/outbox.ts:17-43`

```ts
type Delivery = "sending" | "accepted" | "unknown" | "failed" | "seen";
type OutboxRecovery = { key: string /*1..200 chars*/; value: string };   // byteSize(recovery) <= 64 KiB (outbox.ts:20-32)
type OutgoingEvent = {
  event: EventData;        // {id,pubkey,created_at,kind,content,tags} - unsigned template + client-computed id
  signed?: RelayEvent;     // full signed event (adds sig); present once signing completed
  recovery?: OutboxRecovery; // caller-owned recovery state, committed atomically with the op
  acknowledged?: true;     // caller verified the domain outcome (not just delivery)
  guarded?: boolean;       // caller-scoped admission (channel invite) - retry needs a live gate
  delivery: Delivery;
  error?: string;
}
```

IndexedDB row: `{ scope, id, operation }` with keyPath `[scope, id]` (`outbox-storage.ts:118`).

### 6.2 Event construction at `send()` (synchronous, before any I/O)

`outbox.ts:637-766`

| Step | Detail | Line |
|---|---|---|
| Guards | Not closed, no storage error, `kind` an integer in 0..65535 and in `writer.kinds`. Kind 9 needs non-empty trimmed content. Input byteSize <= `OUTBOX_INPUT_MAX_BYTES` = 32 KiB (`budget.ts:1`); catalog kinds allow content <= 65,535 B and event <= 2*65,535+2,048 (`agents/catalog-envelope.ts:5-9`). | 647-674 |
| Pending cap | `snapshot.length >= MAX_PENDING (256)` throws "Too many outstanding operations; resolve or dismiss a pending operation". | 675-678 |
| Recovery uniqueness | A send with recovery throws if the journal is not hydrated yet, or if another pending op already has the same `recovery.key`. | 652-657 |
| Timestamp | For channel-row kinds `[9, 40002, 40008, 40099]` (`membership.ts:6`): `ms = MessageClock.next(channelId)` and `created_at = floor(ms/1000)`, and the tag `["ms", String(ms % 1000)]` is appended. Other kinds use `Date.now()`. | 681-685, 735 |
| Edit (40003) ordering | Throws unless hydrated. `created_at = max(now, prior own 40003 for same e+h target + 1)`. Throws if that is more than 60 s ahead of now. | 686-709 |
| Catalog (NIP-AP) ordering | `created_at = max(now, supersedes+1, own retained same kind+d +1)`. Same 60 s cap. | 710-727 |
| Idempotency tag | `["client-id", crypto.randomUUID()]` is always appended. | 734 |
| Id | `id = getEventHash(template)`, computed locally. The id is fixed before signing; the signer must not change it. | 743, 463-464 |
| Optimistic row | Appended to `snapshot` with `delivery:"sending"` and `notify()` called synchronously. | 747-758 |
| Intent persist | `intent = ready.then(() => persist(id))`, then `schedule(id, intent)`. | 759-764 |

MessageClock (`message-order.ts:3-57`): `MAX_LEAD_MS = 5000` caps how far other authors' future timestamps can push us. `MAX_OWN_LEAD_MS = 60000` caps our own chain; if the own lead exceeds it, the chain resets to wall clock. The result is `ms = max(now, min(latestSeen+1, now+5000), lastOwnSend+1)`. The rendered order is effective ms ascending, then id ascending (`compareMessages`, `message-order.ts:17-25`).

### 6.3 Delivery state machine

`schedule()`/`deliver()` at `outbox.ts:407-564`

Attempt record: `{ expiresAt, previousDelivery, timer, intent?, controller? }` (`outbox.ts:134-141`). The deadline is `timeoutMs = 10_000` from enqueue. It covers queue wait, signing and publishing (`outbox.ts:90,414-416`; see foundation-review.md:22-25). At most 3 attempts are in flight at once (`inflight() >= 3`, `outbox.ts:423`). When an attempt finishes, its `finally` block re-kicks every `snapshot` item with status `sending`, in array order (`outbox.ts:560-562`).

Order inside `deliver(id)`:
1. Await the `intent` persist, or for a retry `persist(id)`. **The unsigned intent is durable before signing** (`outbox.ts:442-449`).
2. `checkAdmission(id)` runs if the op is guarded (`118-122, 453`).
3. **Sign only if `!initial.signed`** (`454-458`). The result is re-verified with `eventDto` (schnorr and id check). The attempt throws "Signer changed the outgoing event" if `signed.id !== id || signed.pubkey !== viewer` (`459-464`).
4. `replace({...current, signed})`, then await `persist(id)`. **The signed bytes are durable before publish** (`467-468`).
5. Optional `preparePublish(signed, signal)` does read-only preflight under the deadline: mention roster check, canvas membership, workflow validation (`session.ts:296-315`). It returns a sync `check()` (`470-474`).
6. `publishing = true; writer.publish(signed)` (`479-484`). Only entering the publisher makes the outcome uncertain (`475-476`).
7. On success: `delivery = (seen already) ? "seen" : "accepted"`, then `onAccepted(signed)`, which starts `confirm()` echo polling (`489-499`; `session.ts:290-292`).

| From | Event | To | Source |
|---|---|---|---|
| (new) | `send()` | `sending` | outbox.ts:751 |
| `sending` | publish resolved | `accepted` (or `seen` if already seen) | 489-497 |
| `sending` | error before publisher entered (sign, persist, prep, admission, timeout while queued) | `failedDelivery(attempt)`, which is `failed` unless the previous state was unknown/accepted/seen | 400-406, 507-517 |
| `sending` | error after publisher entered, not `PublishRejected` (network error, 5xx, timeout abort) | `unknown` | 515-516 |
| `sending` | `PublishRejected` | `failed` (or previous unknown/accepted/seen) | 515-517 |
| any pending | a verified event with the same id observed in reads or live (`observe()`) | `seen`; moved to the `completed` LRU unless it awaits a receipt or has `recovery` | 838-892 |
| `failed`/`unknown` | user `retry(id)` | `sending` with `previousDelivery` carried over | 767-794 |
| any | `dismiss(id)` | removed from the journal | 795-815 |

Error strings: "Delivery timed out; check or retry this message" (`384`). On a retry from unknown or accepted, the error is prefixed "Retry blocked: " (rejected) or "Retry failed: " (other) (`522-529`). Workflow ops get fixed strings (`518-521`).

A failed retry never downgrades evidence: when `previousDelivery` is `unknown`, `accepted` or `seen`, that state is kept (`outbox.ts:136, 400-406, 510-517`).

Rules for what counts as a permanent rejection (`PublishRejected`, which maps to `failed`) are in `transport.ts:1312-1358` (`acceptPublish`):

| Response | Classification |
|---|---|
| HTTP 409 | `PublishRejected("conflict: the relay state changed; reload before writing again")` |
| HTTP 400/401/403/404/413/422 | `PublishRejected("Relay rejected the message (<status>)")` |
| Non-OK with body `sent:false` or `quota:"api"` (correlated `rate-limited:` reason) | `PublishRejected(body.error)` |
| Any other non-OK (5xx, 429 without correlation) | plain `Error` ("could not be confirmed"), which maps to **unknown** |
| 200 but `event_id !== id` or `accepted` not boolean | plain `Error` ("invalid delivery receipt"), which maps to **unknown** |
| 200 with `accepted:false` | `PublishRejected(message)` |
| 200 with `accepted:true` | success; `message` is returned as the receipt |
| `ApiPaused` / `ApiNotSent` from the admission gate (request never sent) | `PublishRejected` (`transport.ts:1224-1225`; `http-admission.ts:6-23`) |
| Mention preflight failure or a membership change during preflight | `PublishRejected` (`session.ts:994-1030`) |

The receipt body is read with a 16 KiB cap and fatal UTF-8 decoding (`receipt.ts:2-21`).

Native writer extra (`native.ts:835-858`): if `event.created_at < now - 15*60`, the writer first queries `{ids:[id], kinds, authors, consistency:"strong"}`. If the event is found, that counts as success. If not, it throws `PublishRejected("This event is too old to retry and was not found on the relay...")`. The reason is that the relay rejects events outside ±900 s before it deduplicates (`buzz:crates/buzz-relay/src/handlers/ingest.rs:2449-2452`, `MAX_TIMESTAMP_DRIFT_SECS = 900`). **Retries never re-date or re-sign an event.**

Server dedupe by id: re-publishing an existing event returns `accepted:true, message:"duplicate:"` (`buzz:crates/buzz-relay/src/handlers/ingest.rs:3457-3461`). The exception is reactions: a duplicate reaction returns `accepted:false, "duplicate: reaction already exists"` (`buzz:crates/buzz-relay/src/handlers/ingest.rs:3288-3293`), which the client classifies as `failed`. See §9.3 #3.

### 6.4 Echo confirmation polling

`session.ts:579-602`

After `onAccepted`, `confirm(event)` runs `verified.read([{ids:[id],limit:1}])` (foreground first, then background). If the op is still in the snapshot and not `seen`, it re-polls after **[500, 1500, 4000, 10000] ms**, for at most 4 retries (`attempt >= 4` stops). After that, an `accepted` op stays pending in `accepted` state with no further automatic work. Any verified read or live event with that id goes through `writes.observe(visible)` (`session.ts:561`) and marks it `seen`.

Product feedback kind (`PRODUCT_FEEDBACK_KIND`) is never confirmed by echo (`session.ts:291`).

### 6.5 Persistence mechanics

`outbox.ts:205-276`; `outbox-storage.ts`

- Every persist is serialized on one promise chain (`durable`). Each commit writes the **current full `visible` list** (completed LRU plus pending). It does not write a per-op delta, so a commit queued before hydration cannot erase restored intent (`outbox.ts:216-231`; incident in foundation-review.md:26-31).
- Size gate: the commit fails with "Outbox storage is full" if pending bytes exceed 2 MiB (`outbox.ts:237-238`).
- `dismiss` removes the op from memory only **after** the storage commit succeeds (`240-249`). `acknowledge` clears `recovery` and sets `acknowledged:true` only after it is durable (`250-271`).
- A failed status save is swallowed. "Durable intent restores as unknown" (`286-291`).
- The IndexedDB save is incremental. It skips rows whose `delivery`, `error`, `acknowledged`, `signed.id`, `recovery.key/value` and `guarded` are all unchanged. It deletes rows that are no longer present, and writes the partition marker `true`. All of this happens in one `durability:"strict"` transaction (`outbox-storage.ts:97-131`).
- Confirmed retention: `completed = ByteLru(2048 entries, 8 MiB)` (`outbox.ts:127`). Confirmed ops are persisted too (as `visible`). They feed edit and catalog timestamp monotonicity (`693-703, 716-722`) and act as visibility evidence (`session.ts:333-349`).

### 6.6 Restart / hydration

`outbox.ts:292-379`

1. `storage.load()`. `validateSaved` requires at most 2304 records (`MAX_PENDING + MAX_CONFIRMED`) and at most 10 MiB, otherwise it throws "Saved outbox exceeds its budget" (`292-299`).
2. Records are decoded in batches of 12, with `yieldToHost()` (MessageChannel) between batches (`353-359`; `yield.ts`). `decode()` re-verifies `signed` with a full signature check via `eventDto`. For unsigned records it checks `getEventHash(event) === event.id`. Both paths require `pubkey === viewer` (`300-306`).
3. State mapping on restore (`318-323`):

| Saved | Restored |
|---|---|
| `seen` **with** `signed` | `seen` |
| `failed` | `failed` |
| `sending`, `accepted`, `unknown`, or `seen` without `signed` | `unknown` |

4. Partition (`restore()`, `327-347`):
   - **pending** = everything except (acknowledged 9000 or `seen`), plus anything that has `recovery`. If pending exceeds 256, it throws.
   - **completed** = (acknowledged 9000 or `seen`) without `recovery`. If access was revoked during hydration (`confirmedInvalidated`), only creation receipts that match `confirmedKeep` are kept.
5. **Nothing is auto-replayed.** `restore()` never calls `schedule()`. Restored ops stay `unknown`/`failed` until the user calls `retry()` (`outbox.ts:117` comment: "Restored writes never auto-replay"; hardening-validation.md:231).
6. A load error sets `storageError = "Could not load the outbox: ..."`. After that, `send()` and `ready()` throw (`375-378, 650`).

Legacy migration (`outbox-storage.ts:57-95`):
- If the new `events` store has rows for the scope, or `partitions[scope] === true`, those rows are used.
- Otherwise `partitions[scope]` is used if it holds an array (IndexedDB v1 whole-journal layout).
- Otherwise `localStorage["buzz-outbox-v1:"+scope]` (JSON array) is used.
- The first save migrates the data and writes the `true` marker.

### 6.7 User actions and UI states

| UI | Condition | Actions | Source |
|---|---|---|---|
| Message row `DeliveryNotice` | Hidden while `seen`, or until `createdAt*1000 + 10_000` (`DELIVERY_GRACE_MS`). Then `failed` shows "Couldn't send this message."; other states show "Delivery not yet confirmed." + error. | **Retry** button for `failed`/`unknown`. **No discard button** for ordinary messages. | messages/delivery.ts:3-18; DeliveryNotice.tsx:7-39 |
| Edit/delete status (`MessageManagementStatus`) | Pending 40003, or kind 5 with `k` in {9, 40002}, for this channel, in state `failed` or `unknown` | "Retry message update"; "Discard failed update" / "Dismiss unconfirmed update" (unknown: "This may already have reached the conversation... dismissing does not undo it") | MessageManagement.tsx:484-551 |
| Delete dialog | Tracks one kind-5 op | Retry same op ("Retry sends the same operation") | MessageManagement.tsx:395-481 |
| Reactions | Pending reaction in `sending`, `failed` or `unknown` blocks toggling that message's reactions | "Couldn't update reaction." / "Reaction delivery not confirmed." + **Retry reaction** | MessageReactions.tsx:68-72; ReactionTool.tsx:145-166 |

`retry(id)` is refused in these cases: closed; acknowledged 9000; **workflow operations** (`isWorkflowOperation`, inspect/dismiss only); an attempt already running; a dismiss in flight; guarded without a live gate (`outbox.ts:767-794`).

`dismiss(id)` is a no-op while an attempt is running. It is rejected when `recovery` is set and `delivery !== "failed"` ("Confirm this message in New message before removing it") (`outbox.ts:795-815`).

`recover(id, recovery)` attaches caller recovery to an existing receipt (`587-620`). `acknowledge(id)` requires `accepted`/`seen`, except a `member-add:` 9000 (`621-636`). Callers include `channel-members/members.ts:167-219`, `sessions/share.ts:189-264`, `direct-messages.ts:151-171` and `work-sessions.ts:77-159`.

### 6.8 Interaction with edits/deletes and ordering

- **You cannot edit or delete a pending message.** `lastEditableMessage` requires the row delivery to be absent, `accepted` or `seen`. It also requires no pending 40003 (not accepted or seen) and no kind 5 at all targeting that row (`messages/useMessageEdit.ts:13-38`). `useManagedMessage` hides the management UI unless delivery is absent, `accepted` or `seen` (`MessageManagement.tsx:210-223`). `sendToChannel` refuses unconfirmed rows (`relay/messages.ts:113`).
- The timeline fold includes pending local ops for the channel. **Failed non-row ops (edits, deletes, reactions) are excluded from the fold.** Failed kinds 9, 40002 and 40008 stay visible (`store.ts:285-297`).
- Ordering: publication is concurrent, up to 3 at a time, and order of arrival at the relay is **not** guaranteed. The rendered order is fixed at send time by `created_at` plus the `ms` tag, and a retry re-publishes the same bytes, so the row keeps its original position. Edits are made strictly monotonic per target (`+1 s` rule).
- `client-id` (a UUID) makes byte-identical content produce distinct ids. Retries reuse the id, and the relay dedupes by id.

### 6.9 Revocation and disposal

- `purgeConfirmed(keep)` drops confirmed evidence the viewer can no longer see. Pending intent is **never** dropped. During hydration it fences the old disk load (`outbox.ts:824-837`; called at `session.ts:390-394`, which keeps creation receipts).
- Pending ops for revoked channels stay in the journal but are filtered out of views by `local()` (`session.ts:351-357`).
- `dispose()` freezes the final snapshot, aborts attempts, and closes storage after `ready` and `durable` settle. Queued intent is still saved (`outbox.ts:893-913`).
- Multi-window: there is **no** cross-window coordination of the outbox journal. Each window holds an independent in-memory journal over the same IndexedDB partition (foundation-review.md:66-70; no BroadcastChannel or lock in outbox*.ts).

### 6.10 `channel_writes.rs` (host-side narrow command routes, not the outbox)

`src-tauri/src/relay/channel_writes.rs` serves the dedicated routes `channel-details | channel-lifecycle | identity-archive | member-administration`. These are not used by the message outbox (`transport.ts:104-109`).

- `relay_channel_sign(community, route, event)` validates the exact template shape per route and refuses to let member-administration target the viewer. It then signs with the host key (`:145-160`).
- `relay_channel_publish` re-validates, checks `pubkey == viewer`, **verifies the signature** (`verify_signature`, `relay.rs:600-606`), caps the body at `MAX_BODY`, and POSTs to `/events` (`:162-177`).
- Shape rules (`:18-126`):
  - 9002 details: `h` is a uuid; name is at most 120 chars with no leading `#` or whitespace; about is at most 1000 chars and must not contain "Buzz session ("; optional `visibility` (private/open) and `ttl` (positive i32, no leading 0).
  - Lifecycle kinds 9002/9008/9021/9022/41012 take only `h` (9002 adds `archived` true/false).
  - 9035/9036 take `["-"]`, `["p",hex]` and an optional `["auth",hex,_,128-hex]`.
  - 9000/9001: 9000 takes `h`, `p` and `role` (admin|member|guest); 9001 takes `h` and `p`.
- `relay_direct_message` signs 41010 with `p` x1..8 plus `["client",uuid]`, publishes it, and parses `message` "response:{channel_id}" (`:274-335`).

---

## 7. Local persistence inventory

The partition string is `relayPartition(origin, viewer) = "${origin}:${viewer}"`. `splitPartition` splits on the last `:` (`partition.ts:1-33`). Transports without `scope` use `relayAuthor` as the origin (`partition.ts:27-29`).

### 7.1 IndexedDB

| DB (version) | Store / key | Value | Limits / TTL | Cleared by | Source |
|---|---|---|---|---|---|
| `buzz-outbox-v2` (2) | `events`, keyPath `[scope,id]`, index `scope` | `{scope, id, operation: OutgoingEvent}` | 256 pending, 2048/8 MiB confirmed, 2 MiB pending bytes per save, load cap 10 MiB / 2304 rows | `purgeOutboxStorage(scope)` on community leave | outbox-storage.ts:3-49 |
| | `partitions`, out-of-line key = scope | `true` (migrated marker) or legacy array | | same | |
| `buzz-channel-heads-v1` (2) | `heads`, keyPath `key` = `"${viewer}:${relay}:${channelId}"` (**note: viewer first**) | `SavedHead {channelId, savedAt, events[], profiles[]} + {key, scope, bytes}`; signed wire records only, re-verified on load | **Global** across identities: 8 MiB total, 64 heads, 24 h TTL; LRU by savedAt on write | `heads.clear()` on leave; `remove(channelId)` on channel denial (`store.ts:508`); `retain([])` drop (`store.ts:2331`) | persistence.ts:31-247 |
| | `startup`, keyPath `key` = `"${viewer}:${relay}"` | `{key, data: SavedStartup{discovery?{savedAt,relayAuthor,events,profiles}, preferences?{savedAt,data}}, bytes, savedAt}` | 8 MiB total, evicts oldest | same `clear()` | persistence.ts:4-12,122-156 |
| `buzz-read-state-v1` (1) | `partitions`, out-of-line key = partition | `ReadJournal {version:1, slot (32-hex), clientId, state{frontiers,overrides}, localUnread, recent?, reserve?, homes?, revision, acceptedRevision, lastCreatedAt, pending?{event,revision}}`; strictly validated, and a corrupt record is an error, not a reset | localUnread and recent at most 10,000 keys each; reserve bounded by `READ_RESERVE_KEYS/BYTES` | `purgeReadStateStorage` on leave | read-state-storage.ts:19-281 |

Open timeouts: heads uses 1.5 s to open and 2 s per transaction (`persistence.ts:52-55, 93-107`). The outbox and read-state databases have no timeout. Their `onblocked` handlers reject with "Close other Buzz windows..." (`outbox-storage.ts:20-21`, `read-state-storage.ts:207-208`).

Read-state also uses `navigator.locks` `buzz-read-state:${partition}` (exclusive publisher) and `BroadcastChannel` `buzz-read-state:${partition}` (`read-state.ts:56-66,157`; `session.ts:838-848`).

### 7.2 localStorage / sessionStorage

| Key | Value | Scope | Cleared on leave? | Source |
|---|---|---|---|---|
| `buzz-outbox-v1:<partition>` | legacy outbox JSON array | partition | yes (`purgeOutboxStorage`) | outbox-storage.ts:28,80 |
| `buzz-view.v1:` + `JSON.stringify([partition, key])` | arbitrary JSON view intent. Keys include: `draft:<channel>[:thread:<root64>]` (regex `view-state.ts:3-4`), `channel`, `selected-channel`, `scroll:<channelId>`, `direct-message:recipients`, `direct-message:pending`, `hidden-dms`, `search-usage`, `channel-sidebar`, `sessions:channel-drafts`, `<draftKey>:pending/settings/new-draft/new-agent`, `canvas-draft-v1:<ch>`, `todos-draft-v1:<ch>`, `channel-privacy-confirmation:<choice>:dismissed`, `session-retry` | partition | yes (`clearViewScope`) | shared/view-state.ts:44-185 |
| `buzz-channel-setup.v2:<partition>:<id>` | `Progress {version:2,id,input,created,groupDone,canvasDone,added[],accepted[],operations{}}` | partition | yes (`forgetChannelSetups`) | channel-templates/setup.ts:19-43 |
| `buzz.quick-reactions.v1:<partition>` | `[{emoji,count,used}]` | partition | yes | messages/quick-reactions.ts:4-60 |
| `buzz.thread-follows.v1:<partition>` | `[[ "channel:root", bool ]...]`, max 1000 | partition | yes | relay/thread-follows.ts:14-67 |
| `buzz-reminders-checked.v1:<origin>:<viewer>` | unix seconds | partition | **no** | bundled/reminders/index.tsx:29-46 |
| `buzz.catalog-added.v2:` + `JSON.stringify([scope,viewer])` | JSON object | community+viewer | no | bundled/agents/CommunityCatalog.tsx:340-386 |
| `buzz-client.v1:<viewer>` | `{profile{name,picture,about}, memberships[], selected}`, the community list (the only copy in some modes) | viewer | n/a | communities/service.ts:102-110,180 |
| `buzz-community-joins.v1:<viewer>` | `[{id, community, profile?}]` (no invite codes or keys) | viewer | — | communities/join-journal.ts:4-50 |
| `buzz-presence.v1:<viewer>` | presence preference string | viewer | — | presence/activity.ts:32-153 |
| `buzz-notification-preferences.v1:<viewer\|device>` | `{enabled, notifyWhileViewing, sound, categories{}, sounds?}` | viewer | — | notifications/preferences.ts:26-141 |
| `buzz-shortcut-bindings.v1` | bindings JSON | device | — | shortcuts/preferences.ts:4-92 |
| `buzz-appearance.v1`, `buzz-font-scale.v1` | theme pref / scale | device | — | shared/theme/service.ts:4-186 |
| `buzz-design-viewer-color-scheme` | scheme | device | — | design-system/theme/useColorScheme.ts:3-59 |
| `buzz-remember-mentioned-agents.v1` | `"on"`/`"off"` | device | — | messages/mention-preferences.ts:3-35 |
| `buzz.video.playback-speed` | number | device | — | messages/VideoPlayer.tsx:48-397 |
| `buzz:klipy-customer-id:v1` | random id | device | — | relay/gifs.ts:48-125 |
| `buzzodz.plugins.v1` (+ `.backup.<ms>` on corruption) | plugin enablement | device | — | plugins/storage.ts:10-92 |
| `buzz.agents2.v1` | `{version:1, agents{pubkey: AgentRecord}}` | device | — | agents2/store.ts:38-130 |
| `buzz.claude-code.sessions.v1` | sessions map | device | — | agent-plugins/claude-code/sessions.ts:276-295 |
| `buzz-team-import:<sha256hex>` | import progress (removed on complete) | device | — | agents/team-bundles.ts:79-100 |
| `buzz.hosted-community-deletion.v1` | `PendingDeletion` | device | — | bundled/hosted-communities/api.ts:5-367 |
| `buzz.builderlab.enrollment.v1:*`, `buzz.builderlab.deletion.v1:*`, `buzz.builderlab.agent:<ctx>:<pubkey>`, `buzz.builderlab.registration.v1:<json>` | plugin intent | device | — | bundled/builderlab/agents/*.ts |
| `emoji-mart.frequently`, `emoji-mart.last` | library state (removed if `{}`) | device | — | bundled/emoji/emoji-mart.ts:98-100 |
| sessionStorage `buzz-navigation-end:<chainUUID>` | forward-history end index | tab | — | navigation/browser-history.ts:74-174 |
| `history.state.buzzNavigationV1` (not storage) | `{chain,index,entry}` | tab | — | browser-history.ts:10 |

BroadcastChannel `buzz-presence-input` (`presence/activity.ts:52`).

### 7.3 Tauri-side files and secrets

| Item | Location | Content | Source |
|---|---|---|---|
| Human key | OS keychain, service `dev.local.buzz.foundation.identity` (`.debug` in debug builds), account `human` | nsec | crates/credential-store/src/lib.rs:15-24 |
| Agent keys | keychain service `dev.local.buzz.foundation.agents` | agent nsecs | crates/agent-controller/src/credentials.rs:16 |
| Archive DB | `app_data_dir/archive[-debug]/events.sqlite3` | Agent observer/metrics archive (kinds 24200/44200 only). Tables `archive_partitions(viewer,community,revision)`, `archive_subscriptions(...,name,enabled,days,budget)`, `archive_events(seq,viewer,community,subscription,id,agent,kind,created,received,bytes,envelope, UNIQUE(viewer,community,id))`, `archive_usage` with triggers. `user_version=2`, refuses >2; WAL; `secure_delete=ON`; `auto_vacuum=INCREMENTAL`. Defaults: observer 30 d / 512 MiB, metrics 90 d / 64 MiB; prune at most every 60 s. | archive/mod.rs:40-62; archive/store.rs:25-140; src/features/archive/schema.sql |
| Agent controller | `app_data_dir/agent-controller/` (`agents.json`, `defaults.json` kept on erase unless removing agents; install logs) | | lib.rs:640-650; sign_out.rs:39-43 |
| App agents | `app_data_dir/agents2/identities.json` | | lib.rs:668-672 |
| Upload spools | `app_cache_dir/upload-spools/` (`source`, `ownership` files), reaped every 15 s | | lib.rs:613-627; relay/upload_spool.rs |
| Window geometry | `app_config_dir/<filename>` | | window_state.rs:55-62 |
| Sign-out marker | `data_dir/.{identifier}--{HUMAN_SERVICE}.sign-out-pending` (JSON `{wipe, removeAgents}`), `.{service}.sign-outs-finished` (1 byte per finish), locks `.{service}.instance.lock`, `.dev.local.buzz.foundation.instance.lock`, `dev.local.buzz.agent-ownership` | | sign_out.rs:39-120,610-614 |

### 7.4 When data is cleared

- **Leave community** (`communities/service.ts:333-376`): dispose the session, then `purgeCommunityDeviceState(origin, viewer)` (`communities/device-state.ts:31-50`). That clears view state, channel setups, quick reactions, thread follows, then channel heads, read state and outbox (`partition-purge.ts:9-28`). Failures are logged and returned, never retried. With `purge:false` (leave refused because the viewer is banned, `CommunityRail.tsx:157-159`) the device state is kept.
- **Sign out (plain)**: the next launch removes **only the human keychain key**. IndexedDB and localStorage stay; they are keyed by viewer (`sign_out.rs:546-608`: `targets` is empty unless `wipe`). The JS side only invokes `sign_out` (`identity/service.ts:77-78`).
- **Sign out + erase (`wipe`)**:
  1. On the next launch, before any window opens, take the exclusive locks.
  2. Remove agent keys if asked.
  3. Move `app_data` (keeping `agent-controller` unless `remove_agents`), `data_local_dir/<id>`, `cache_dir/<id>` and `~/Library/WebKit/<id>` aside.
  4. Remove the human key.
  5. Delete the moved data.
  6. Append to the finished record and remove the marker.

  Any failure rolls the moves back and keeps the marker, so the wipe retries next launch (`sign_out.rs:1-8, 546-608`). Erase is refused in debug builds and when `BUZZODZ_HOME` is set; removing agents requires an erase (`sign_out.rs:716-737`).
- Channel denial: `persistence.remove(channelId)` (`store.ts:508`). Outbox pending is kept.

### 7.5 Recommended SQLite schema (native port)

One file per app install, e.g. `app_data/buzz.sqlite3`, with `PRAGMA journal_mode=WAL; synchronous=FULL` for the outbox (it replaces IndexedDB `durability:"strict"`) and `secure_delete=ON`. A `partition` is `(origin TEXT, viewer TEXT)`.

```sql
PRAGMA user_version = 1;

-- 6. Outbox: one row per op; replaces buzz-outbox-v2.events
CREATE TABLE outbox_ops (
  origin        TEXT NOT NULL,
  viewer        TEXT NOT NULL,
  event_id      TEXT NOT NULL CHECK(length(event_id)=64),
  kind          INTEGER NOT NULL,
  created_at    INTEGER NOT NULL,
  channel_id    TEXT,                        -- 'h' tag, for view lookup & revocation filtering
  template_json TEXT NOT NULL,               -- EventData (unsigned, incl. client-id/ms tags); id = hash(template)
  signed_json   TEXT,                        -- exact signed bytes; NEVER regenerated once set
  delivery      TEXT NOT NULL CHECK(delivery IN ('sending','accepted','unknown','failed','seen')),
  error         TEXT,
  recovery_key  TEXT CHECK(recovery_key IS NULL OR length(recovery_key) BETWEEN 1 AND 200),
  recovery_value TEXT,                       -- <= 64 KiB
  acknowledged  INTEGER NOT NULL DEFAULT 0,
  guarded       INTEGER NOT NULL DEFAULT 0,
  confirmed     INTEGER NOT NULL DEFAULT 0,  -- 1 = in 'completed' retention (seen/acked w/o recovery)
  enqueued_seq  INTEGER NOT NULL,            -- monotonic per partition; preserves snapshot order
  bytes         INTEGER NOT NULL,
  updated_ms    INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer, event_id)
);
CREATE INDEX outbox_pending ON outbox_ops(origin, viewer, confirmed, enqueued_seq);
CREATE UNIQUE INDEX outbox_recovery ON outbox_ops(origin, viewer, recovery_key)
  WHERE recovery_key IS NOT NULL AND confirmed = 0;
-- Enforce in code: <=256 pending, <=2048 / 8 MiB confirmed (LRU by updated_ms), pending bytes <=2 MiB.

-- Channel heads cache (disposable; re-verify signatures on load)
CREATE TABLE head_events (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, channel_id TEXT NOT NULL,
  event_id TEXT NOT NULL, created_at INTEGER NOT NULL, kind INTEGER NOT NULL,
  raw_json TEXT NOT NULL,                    -- signed wire event
  PRIMARY KEY (origin, viewer, channel_id, event_id)
);
CREATE TABLE head_meta (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, channel_id TEXT NOT NULL,
  saved_ms INTEGER NOT NULL, bytes INTEGER NOT NULL, profiles_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer, channel_id)
);  -- global budget: 8 MiB, 64 heads, 24 h TTL
CREATE TABLE startup_cache (
  origin TEXT NOT NULL, viewer TEXT NOT NULL,
  discovery_json TEXT, preferences_json TEXT, saved_ms INTEGER NOT NULL, bytes INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer)
);

-- Read state journal (one row per partition, validated as a whole; corrupt => error)
CREATE TABLE read_state (
  origin TEXT NOT NULL, viewer TEXT NOT NULL,
  journal_json TEXT NOT NULL,                -- ReadJournal v1 (slot, clientId, state, localUnread, recent, reserve, homes, revisions, pending)
  PRIMARY KEY (origin, viewer)
);

-- View intent (drafts, selection, scroll, DM recipients...). Keys as in 7.2.
CREATE TABLE view_state (
  origin TEXT NOT NULL, viewer TEXT NOT NULL, key TEXT NOT NULL,
  value_json TEXT NOT NULL,                  -- drafts up to 8 MiB
  updated_ms INTEGER NOT NULL,
  PRIMARY KEY (origin, viewer, key)
);
CREATE TABLE partition_prefs (                -- quick reactions, thread follows, channel setups, reminders-checked, catalog-added
  origin TEXT NOT NULL, viewer TEXT NOT NULL, name TEXT NOT NULL, value_json TEXT NOT NULL,
  PRIMARY KEY (origin, viewer, name)
);
CREATE TABLE viewer_prefs (                   -- client (memberships/profile/selected), joins, presence, notifications
  viewer TEXT NOT NULL, name TEXT NOT NULL, value_json TEXT NOT NULL,
  PRIMARY KEY (viewer, name)
);
CREATE TABLE device_prefs (name TEXT PRIMARY KEY, value_json TEXT NOT NULL);
```

Leaving a community becomes one transaction: `DELETE FROM {outbox_ops, head_*, startup_cache, read_state, view_state, partition_prefs} WHERE origin=? AND viewer=?`. The current app clears these stores one at a time and logs failures. A port can do it in one transaction, but it must still dispose the session first so nothing writes them again. Keep the agent archive as a separate DB. It has its own retention and erase semantics.

---

## 8. Event validation and access

### 8.1 Signature and id verification

`src/features/relay/events.ts:45-133`

- **Every inbound event goes through `checkedEvent`.**
  - Structural checks: `created_at` is a safe non-negative integer; `kind` is an integer in 0..65535; `id` and `pubkey` are 64 lowercase hex; `sig` is 128 lowercase hex; `content` is a string; tags are arrays of strings.
  - The wire fields are copied into a fresh object. **A caller-supplied `verifiedSymbol` is never trusted.** Then `verifyEvent` checks the hash against `id` and the schnorr signature.
  - The object is deep-frozen and added to a module `WeakSet` (`hasEventProof`).
  - Error: "Relay supplied a malformed or invalidly signed event".
- `eventsDto`: **one bad event rejects the whole response** (`events.ts:56-61`).
- `createEventVerifier()` is a per-connection proof memo: `ByteLru(2048, 2048*192)` mapping id to sig. A hit still re-hashes, and requires the hash to match and the signature to be identical (`events.ts:63-81`).
- Where verification is applied:
  - HTTP reads: `parseEvents(raw, verify)` in batches of 12 with a yield between them (`transport.ts:221-240, 1040-1042, 1080-1081`).
  - Live WS/SSE: `eventDto` (`live.ts:807,939`; `broker-live.ts:166-184`).
  - Signer output: re-verified (`outbox.ts:459-461`; `native.ts:290-297`).
  - Restored outbox: signed records re-verified, unsigned records hash-checked (`outbox.ts:300-306`).
  - Host publish route: Rust `verify_signature` (`relay.rs:600-606`).
- Tie-break for replaceables: `newer()` prefers the later `created_at`; on a tie, the **lower id** wins (`events.ts:140-150`).

### 8.2 Relay-signed events (authority = NIP-11 `self`)

The authority is the community's NIP-11 `self`, never a contact-key fallback.
- Native: `nativeRelayInfo(origin)` does GET `/`. `self` must match `/^[a-f0-9]{64}$/`, otherwise the connection fails with "Relay did not advertise its identity" (`native.ts:314-334`). The value is passed as `relayAuthor` to `connectSignedTransport` (`transport.ts:1117-1121`).
- Broker mode: `/session.relayAuthor` must be a string. `archiveAuthority`, if present, must equal it (`transport.ts:424-434`).

Kinds the client accepts **only** when `pubkey === relayAuthor` (others are ignored or rejected):

| Kind | Meaning | Check site | On mismatch |
|---|---|---|---|
| 39000 | group metadata | discovery.ts:41-44; store.ts:1083,1367-1405,1459 | ignored |
| 39001 | admins | channel-lifecycle-protocol.ts:91-117; channel-details-protocol.ts:109 | **throws** "Channel state did not match the requested authority" |
| 39002 | member roster | discovery.ts:41-44; store.ts:1070-1073,1155-1165 (whole batch rejected as invalid-response); session.ts:1046-1080 (mention preflight); work-sessions.ts:292-307 | ignored / throws |
| 39005 | thread summaries | fold.ts:261-264 | ignored |
| 39006 | channel window bounds | window.ts:40-46 | page invalid |
| 39007 | thread window bounds | thread-window.ts:108-140 | invalid |
| 40099 | membership activity rows | membership.ts:12-17 | ignored |
| 13534 | NIP-43 community roster (`["member",pk,role]`) | communities/roster.ts:13-30; identity-archives.ts:151-176 | **throws** "Member list is not signed by this community" |
| 13535 | identity archive snapshot (NIP-IA) | identity-archives.ts:82-99 | error state |
| 20001 | presence snapshot | transport.ts:330-338 | throws "Untrusted presence snapshot" |
| 9 (relay-signed) | workflow/agent output attributed via `actor` tag or leading `p` | fold.ts:199-215; workflow-attribution.ts:9; unread.ts:378-386 | treated as relay author |

39003 and 39004 are **not referenced** in client source (grep found nothing).

### 8.3 Visibility filter for auxiliary events

`event-access.ts:3-44`

`eventVisibility(canAccess, find)`:
- An event is visible only if every `h` tag (plus the `d` of 39000/39001/39002) is accessible.
- For the AUXILIARY kinds `{5, 7, 9005, 40003, 39005}`, it must also have at least one channel or target. Every `e` target must be **known and itself visible**, checked recursively with a depth cap of 32 and cycle protection.
- An orphan auxiliary is hidden. An explicit `#h` cannot vouch for an unknown second target.

### 8.4 Access revocation: detection

| Signal | Handling | Source |
|---|---|---|
| Live CLOSED `"restricted: not a channel member"` | `callbacks.denied(id)` → `channels.denyChannel(id)` for each channel in the route scope | live.ts:504-515; session.ts:2559-2561 |
| Live CLOSED `"restricted: channel access revoked"` (server: buzz:crates/buzz-relay/src/handlers/side_effects.rs:264) | **Hint only**: triggers `refreshRoster()`, and only for a newly erroring route | session.ts:2474-2489 |
| HTTP read 401/403 (`ReadError.kind="denied"`) on channel reads | `denyChannel(channelId)` | store.ts:584-586, 665-667, 1345-1347; session.ts:449-454, 1810-1812 |
| 401/403 on the initial roster/discovery list | `discovery.denyAll()`; revoke all channels, evict windows, clear heads and tails | store.ts:1040,1217-1226 |
| Complete viewer-scoped 39002 omission for a cached channel | `denyChannel(id, "Conversation is unavailable")` | store.ts:1383-1396 |
| Missing 39000 for a read-only or suspended preview | deny | store.ts:1398-1411 |
| Channel lifecycle reports removal | `denyChannel(id, "Channel is no longer available")` | session.ts:764-765 |
| Discovery access revision changed | `transport.revokeAccess(commit)` | store.ts:975-978 |
| 13534 community roster | **Not** used for revocation. It shapes UI roles and archive consent only | communities/roster.ts:1-2 |
| Community-level removal | No automatic purge. The user-initiated `leave()` purges. A banned viewer leaves with `purge:false` | CommunityRail.tsx:150-160 |

Partial discovery or a transport failure is **not** proof of removal (hardening-plan.md:81-84).

### 8.5 Access revocation: what is purged

`denyChannel` (`store.ts:476-511`):
- Bumps `accessVersions[id]`, then calls `discovery.deny` and removes the id from `allowed`.
- Inside `revokeAccess`: deletes heads and tails; bumps the window generation and aborts it; empties events and traffic; sets the window to `error`; clears the profile directory; drops the channel from the list; calls `persistence.remove(channelId)`; saves discovery.

Session `revokeAccess(commit)` (`session.ts:361-416`):
- Bumps `accessEpoch`, which fences late reads (`epoch !== accessEpoch` checks, e.g. `session.ts:457,498,544-562`).
- Cancels uploads, lifecycle, details, member administration and typing.
- Invalidates request dedupe for every filter except owner-inventory.
- Purges `recent` events that are no longer visible, plus **all** kind 0 events.
- Calls `channels.purgeAccess` and `writes.purgeConfirmed(keep visible or creation receipts)`.
- Clears profiles, emoji, statuses, memories, presence, channel activity, archives and workflows. Purges every view, then `commit()`, `unread.purge()` and `inboxFeed.purge()`.
- Notifications are deferred until `revoking === 0`, so callbacks cannot re-seed denied data.

**Pending outbox intent survives** (signed bytes and id unchanged). It is hidden from views (`session.ts:351-357`; hardening-validation.md:101-103, 232).

### 8.6 Identity archives (NIP-IA)

`identity-archives.ts`; `identity-archive-protocol.ts`; host route `channel_writes.rs:81-107`

- Read: `{kinds:[13535], authors:[authority], limit:1}`, fresh, background.
  - Valid only if there is exactly 1 event, `pubkey === authority`, content is empty, there is exactly one `["-"]` tag, and the event is at most 2 MiB.
  - The archived set is the `p` tags (hex), deduplicated and sorted.
  - **A session-local rollback fence** rejects an older snapshot, or an equal-time snapshot with a higher id, as "Stale archive snapshot" (`identity-archives.ts:56-122`).
  - Errors give status `error` and visibility **fails open**: `archiveHides` hides only when the state is known to be `archived`, and never hides the viewer from themself (`:25-34`).
- Consent (`:141-179`):
  - Self is always allowed.
  - Otherwise, read the target's kind 0 `auth` tag (NIP-OA owner must be the viewer, `authTagOwner`).
  - Or the viewer has role owner/admin in the latest relay-signed 13534.
  - Admin authority sends **no** `auth` tag.
- Request (`:180-233`):
  - Template is kind 9035 (archive) or 9036 (unarchive) with tags `[["-"],["p",target],(["auth",owner,cond,sig128])?]` and empty content (`identity-archive-protocol.ts:8-48`).
  - 20 s timeout. The signed result must match the template byte-for-byte, otherwise "Signer changed the archive request".
  - Publish: `PublishRejected` → throw. Any other error is **uncertain**, so re-read the snapshot and confirm the expected state. Otherwise throw the uncertain error, or "The relay did not confirm the change. Retry."
- Not live-subscribed. Cleared on revocation (`session.ts:403`).

### 8.7 Tauri host validation and command surface (reference)

#### 8.7.1 What the Rust host verifies
- **Incoming events are NOT verified in Rust.** `relay_http` returns the raw body string (`relay.rs:654-706`). All read-path verification happens in TS:
  - `createEventVerifier`/`checkedEvent` (`src/features/relay/events.ts:66-130`): strict field types, a safe-integer `created_at`, kind 0-65535, lowercase hex `id`/`pubkey`/`sig`, string tags.
  - `id` is recomputed (`getEventHash`) and the Schnorr signature verified (`verifyEvent`), with a 2048-entry memo keyed by id+sig.
  - A single bad event rejects the whole `/query` response. Verification is batched 12 at a time (`transport.ts:206-230`).
  - A native port should do this in Rust.
- **Relay-authored kinds** are accepted only when `pubkey == NIP-11 self` (TS checks):

| kind | meaning | check location |
|---|---|---|
| 13534 | membership roster | `roster.ts:17` |
| 39000 | group metadata | `relay/store.ts:1317,1446`, `direct-messages.ts:79` |
| 39002 | group members | `relay/store.ts:1143,1326,1569`, `session.ts:1046`, `work-sessions.ts:290-377` |
| 39005 | thread summary | `relay/fold.ts:262-263` |
| 39006 | window bounds (`h`, `d` tags) | `relay/window.ts:42-46`, `thread-window.ts:134` |
| 40099 | membership notice | `relay/membership.ts:17` |
| 9 with `buzz:workflow` tags | workflow-authored message | `relay/unread.ts:380-385` |

  Channel lifecycle, details and member-administration snapshots are also read with `authors:[relayAuthor]` (`channel-lifecycle-protocol.ts:91`, `channel-details.ts:88`, `channel-members/administration.ts:79`).
- **Outgoing checks in Rust before publish:**
  - `relay_http POST /events`:
    - kind 30078 → `admit_app_data`: `pubkey == viewer`, signature valid, then sidebar-payload or kit-ciphertext validation.
    - kind 30300 → reminder admission.
    - kind 9007 → owned and signature-verified, plus the creation shape.
    - Rejected outright: kind 41010 (use `relay_direct_message`), and kinds 9002/9008/9021/9022/41012/9035/9036 (must go through `relay_channel_publish`).
    - Source: `relay.rs:571-608,672-696`.
  - `relay_channel_publish` / `relay_publish_read_state` verify route shape, `pubkey == viewer` and the signature (`src-tauri/src/relay/channel_writes.rs:130-176`, `relay.rs:549-562`).
  - Decrypt paths (sidebar, read-state, kit, reminders) verify the signature and self-authorship before decrypting (§2.9).

#### 8.7.2 Relay command reference
| command | args | behavior / limits |
|---|---|---|
| `relay_http` | `community, path, method, body?: string` | Allowlist: GET `/`, `/api/join-policy`; POST `/query`, `/events`, `/api/invites`, `/api/invites/claim`, `/api/invites/accept-policy`, `/gifs/search` (`relay.rs:74-94`). Body ≤ 1 MiB and required for POST; GET must have none. POST is NIP-98 signed. Returns `{status, headers{content-type,retry-after,server-timing}, body: string}`; response ≤ 16 MiB and UTF-8. Timeouts 10 s connect / 30 s total; no redirects; no retry. |
| `relay_workflow_runs` | `community, id(uuid), cursor?{before,beforeId}` | Signed `GET /workflows/<id>/runs?limit=20[&before&before_id]`; response ≤ 1 MiB (`relay.rs:111-161`). |
| `relay_sign` | `community, event{kind,created_at,tags,content}` | Validates kind and shape (§2.5); returns the signed event JSON. |
| `relay_kit_sign`, `relay_kit_prepare {community, record}`, `relay_kit_decode {community, events}` | | Channel recipes (30078, NIP-44 to self) (`src-tauri/src/relay/kit.rs:327`, `channel_writes.rs:231-268`). |
| `relay_channel_sign` / `relay_channel_publish` | `community, route, event` | route ∈ `channel-details\|channel-lifecycle\|identity-archive\|member-administration`. 9000/9001 member admin may not target self. Publish POSTs `/events`. |
| `relay_direct_message` | `community, pubkeys[1..8]` | Signs kind 41010 `[["p",..]...,["client",uuid]]`, POSTs it, and requires 200 with `event_id` matching and `accepted:true` (`channel_writes.rs:274-320`). |
| `relay_sign_read_state` `{community, intent{slot,createdAt,blob}}`, `relay_decode_read_state {community, events}`, `relay_publish_read_state {community, event}` | | NIP-44 read state; publish body ≤ 64 KiB. |
| `relay_sign_sidebar {coordinate,payload,created_at}`, `relay_decode_sidebar {events}` | | Sidebar preferences. |
| `relay_sign_reminder {intent}`, `relay_decode_reminders {events}` | | Kind 30300. |
| `relay_git_authorization` | `community, repository` | NIP-98 token for `<origin>/git/<hex>/<name>`. |
| `relay_upload_begin/chunk/upload/cancel` | §5n.2 | |
| `media_download`, `media_copy_image`, `media_snapshot_read`, `media_stream_base`; URI scheme `buzz-media` | §5n.6 | |
| `relay_archive`, `relay_agent_*`, `relay_project_git*` | | Agent and archive features, outside this part. |

Registered in `src-tauri/src/lib.rs` (`generate_handler!`, around line 48); the scheme is registered at `lib.rs:704`.

#### 8.7.3 JS-side timeouts around these commands
| path | timeout | source |
|---|---|---|
| `nativeRelayRequest` | 30 s | `native.ts:99-123` |
| `communityRequest` / `nativeCommunityRequest` | 25 s | `api.ts:26-28`, `native-api.ts:24-27` |
| `inspectProfile` | 12 s | |
| icon | 12 s | |
| presence query | 10 s | |
| live connect/auth deadline | 10 s, then reconnect | `live.ts:698-701` |
| upload | 600 s; plus 600 s more when prepared | `native.ts:252-256` |
| `nativeSignedQuery` | response ≤ 8 MiB | `native-api.ts:270-294` |

IPC cannot abort a reqwest call. JS only fences late results, so a write that was already dispatched may still complete.

---

## 9. Port plan

### 9.1 Recommended Rust module breakdown (`crates/relay`)

The app uses one async runtime (tokio) and runs **one session actor per open community**. Each
actor owns its socket, reader, outbox and stores. UI-facing state is exported as immutable,
`Arc`-shared snapshots, each with a revision number. GPUI entities subscribe to snapshot
changes on a channel. Snapshots must keep their identity (`Arc::ptr_eq`) when the content is
unchanged, which matches `row-identity.ts`, so views can skip re-render.
Projections are updated incrementally (`MessageProjection` model, §5b.3). Do not rebuild them per event.

Identity and signing live in a small separate crate. The human key never leaves it, and it
exposes only purpose-bound operations, mirroring the Tauri allowlist in §2.5 and §8.7.

| Module | Responsibility | Ports from | Key spec |
|---|---|---|---|
| `identity` (crate `buzz-identity`) | keyring storage (service/account names, debug variant); nsec import/generate; `sign(template)` with a kind/shape allowlist; NIP-44 v2 self encrypt/decrypt; builders for NIP-42 (22242), NIP-98 (27235) and Blossom (24242) | `src-tauri/src/identity.rs`, `crates/credential-store`, `src-tauri/src/relay.rs:196-336,708-856` | §2 |
| `relay::event` | `Event` / `UnsignedEvent`; canonical id hash; `VerifiedEvent` newtype that is only constructible by `verify()`; strict field checks; `newer()` replaceable ordering (later `created_at`, tie goes to lower id); tag helpers (`single_h`, `thread_reference`, …); kind constants | `events.ts`, `thread-reference.ts`, `body.ts` | §8.1, §5c.1 |
| `relay::origin` | `relay_origin()` normalization; `wss` URL; partition key `"<origin>:<viewer>"`; thread-window authority host | `communities/destination.ts`, `partition.ts`, `thread-window.ts:17-29` | §1.2 |
| `relay::nip11` | `GET /` with `Accept: application/nostr+json`; parse `self`, `name`, `icon`, `supported_nips`, `read_state_snapshot` and the gif capability; icon sanitizing | `native.ts:315-345`, `community-icon.ts`, `gif-capability.ts` | §1.4–1.5 |
| `relay::http` | NIP-98 client: fresh auth event per request, 10 s connect / 30 s total timeouts, no redirects, no retry after a transport error, response caps; per-principal **admission lane** (6 in flight, queue 128, quota pause on a correlated 429) | `src-tauri/src/relay.rs:640-794`, `http-admission.ts`, `host-admission.ts`, `transport.ts:1211-1360` | §2.7, §4.1, §4.3 |
| `relay::reader` | filter validation and canonicalization; coalescing identical reads; 3 slots with ≤1 background; 10 s deadline from enqueue; 128 pending jobs; `fresh`, `promote` and `invalidate`; batch verification; 8 MiB response cap; thread-window bound verification | `reader.ts`, `thread-window.ts` | §4.3, §5c.4 |
| `relay::live` | WebSocket; AUTH handshake; route/wire model; batching of 10 channels per REQ; capacity of 1022 routes; 4 concurrent setups; EOSE/CLOSED handling; 60 s renewal tick; backoff of `500·2^n` with 5 attempts; quota cooldown; presence publish; answer server Pings | `live.ts`, `socket-requests.ts` | §3 |
| `relay::session` | orchestrator: connect sequence; `accept()` pipeline order; `recent` LRU; `accessEpoch`; reconnect side effects (stale heads, catch-up queue, refresh fan-out); echo confirmation polling | `session.ts`, `service.ts`, `incoming.ts` | §3.5, §3.8, §5 intro |
| `relay::discovery` | roster scan (39002 `#p`), omitted-roster confirmation, 39000 metadata, access state machine (authorized / canAccess / canParticipate / denied), membership hints, cached-roster display-only mode | `discovery.ts`, `store.ts:1015-1411`, `listed-channel.ts`, `membership.ts` | §5a.2–5a.3 |
| `relay::channels` | channel list; history windows (head, older pages, 39006 parsing); prepared heads; live tails; catch-up; overflow and `historyLimited`; window LRU of 3 | `store.ts`, `window.ts`, `session-window.ts`, `projection.ts` | §5b |
| `relay::fold` | `fold_messages` (rows, edits, deletes, reactions, summaries, attachments, mentions, authorship); incremental `MessageProjection` with an overlay index; `compare_messages` with `MessageClock` | `fold.ts`, `message-projection.ts`, `message-order.ts`, `row-identity.ts`, `attachments.ts` (read side) | §5b.2–5b.5 |
| `relay::threads` | thread views (probe/strict/legacy modes, 10/50 page sizes, 10 pages max, 2000 events / 4 MiB); exact-message reveal; reply tree; follows | `threads.ts`, `thread-window.ts`, `thread-follows.ts`, `messages/reply-tree.ts` | §5c |
| `relay::messages` | builders for kinds 9, 40003, 5, 7 and 1984; mention tags; emoji tags; imeta; sent-from-thread | `messages.ts`, `emoji.ts`, `attachments.ts` (write side), `bundled/mentions/README.md` | §5d |
| `relay::outbox` | durable journal (SQLite); state machine; 3 in flight; 10 s deadline from enqueue; `client-id` and `ms` tags; edit monotonicity; recovery keys; retry, dismiss and acknowledge; hydration verification; confirmed LRU | `outbox.ts`, `outbox-storage.ts`, `receipt.ts` | §6 |
| `relay::read_state` | journal; NIP-RS blob encode/parse; merge; retention and coverage; 5 s debounced publish with exact-bytes pending; snapshot capability | `read-state*.ts`, `src-tauri/src/identity.rs:573-880` | §5i.1–5i.6 |
| `relay::unread` | evidence store; evidence repair (128-channel batches); unread predicate; snapshots; thread activity; conversation lookups; reading leases; mark actions; inbox projection and inbox feed | `unread.ts`, `inbox.ts`, `inbox-feed.ts`, `channel-activity.ts` | §5i.7–5i.11, §5l.5–5l.6 |
| `relay::notifications` | incoming-message admission, eligibility (`wait`/true/false), dedupe for 120 s, preferences, text | `features/notifications/*` | §5l.1–5l.2 |
| `relay::sidebar_prefs` | 30078 coordinates `channel-sections\|stars\|mutes\|sort`; register model; serialized write queue with strong readback | `sidebar-*.ts`, `me-preferences.ts`, `native-sidebar.ts`, `src-tauri/src/identity.rs:338-750` | §5a.6–5a.8 |
| `relay::dm` | 41010 open, confirmation, 30622 visibility, 41012 hide, local "Remove from Messages", people search | `direct-messages.ts`, `direct-message-policy.ts`, `bundled/channels/useHiddenDms.ts` | §5e |
| `relay::lifecycle` | 9007 create (via outbox), 9021/9022/9008/9002-archive/41012 (direct publish and polling confirm), 9002 details edit with optimistic version | `channel-lifecycle*.ts`, `channel-details*.ts`, `work-sessions.ts` | §5m |
| `relay::members` | channel roster/admin reads (strong), 9000/9001 administration, invite with mention preflight, 40099 grouping | `channel-members/*`, `messages/membership-rows.ts` | §5g |
| `relay::community` | saved communities, join flow (join-policy, accept-policy, claim, profile), invites, leave (28936), 13534 roster, 9030–9032 admin | `features/communities/*` | §1 |
| `relay::profiles` | profile directory (500-author chunks, 1024 entries / 2 MiB, no TTL, refresh on reconnect), kind-0 fold, own-profile publish, 10100 | `profile-directory.ts`, `profiles.ts`, `profile-selection.ts` | §5f |
| `relay::presence` / `relay::typing` | presence demand (≤256 subjects), HTTP snapshot, live route, 75 s evidence expiry, renewal every 60–65 s, auto-away; typing receive with 8 s lifetime and 2 s quiet | `features/presence/*`, `transport.ts:307-345,1133-1172`, `typing.ts` | §5h |
| `relay::status` / `relay::emoji` / `relay::reminders` | 30315 watch and save; 30030 catalog union and add; 30300 reminders | `user-status*.ts`, `emoji-directory.ts`, `reminders.ts` | §5j, §5k, §5o |
| `relay::media` | upload (hash, `PUT /upload` with 24242 auth, descriptor validation); download proxy (Blossom `get` auth, Range, 1 MiB block cache); image sanitizing and ffmpeg preparation | `src-tauri/src/relay.rs:818-1235`, `relay/upload_spool.rs`, `media_*.rs`, `attachments.ts`, `attachment-limits.ts`, `messages/prepare-attachment.ts` | §5n |
| `relay::store` | SQLite (WAL): outbox, heads, startup cache, read-state journal, view state, prefs; partition purge in one transaction | `persistence.ts`, `outbox-storage.ts`, `read-state-storage.ts`, `shared/view-state.ts`, `partition-purge.ts` | §7 |

The Tauri app splits work between JS and Rust, so JS has to re-verify Rust output. For example, it
checks that "the signer did not alter the template" and re-verifies `relay_sign` results. In a
single-process Rust app, the identity crate is the only signer, so those self-checks reduce to
debug assertions. **Do keep** the purpose-bound signing surface. Callers ask for "sign read-state
blob" or "sign 41010", never "sign arbitrary event", so a bug in a feature module cannot mint
NIP-98 or Blossom tokens or forge other kinds.

Suggested ordering of work (each step is usable on its own):
1. `event`, `origin`, `nip11`, `identity`, `http`, `reader`.
2. `live`, `session`, `discovery`, `channels` and `fold`, giving a read-only timeline.
3. `outbox` and `messages`, giving send, edit, delete and react.
4. `threads`, `profiles`, `media`.
5. `read_state` and `unread`, then `sidebar_prefs` and `dm`.
6. `presence`, `status` and `emoji`, then `lifecycle`, `members` and `community`, then `notifications`.

### 9.2 Risky and subtle behaviors (from `docs/archive/*` and code comments)

Sources: `docs/archive/foundation-review.md`, `docs/archive/hardening-plan.md`, `docs/archive/hardening-validation.md` and code comments.

| # | Behavior a port must get right | Why / incident | Source |
|---|---|---|---|
| 1 | A verified echo must end the attempt **and free its delivery slot**, even if the HTTP ACK never arrives or the publisher ignores cancellation. | With 3 hanging publishers, a 4th message stayed queued forever. | foundation-review.md:15-21; outbox.ts:883-889 |
| 2 | The delivery deadline starts at **enqueue** and covers queue wait. A duplicate retry must not reset it. | Queued messages escaped the timeout. | foundation-review.md:22-25; outbox.ts:412-418 |
| 3 | Every persist must commit the **current whole journal after hydration and prior commits**, not a snapshot captured earlier. | A status save queued before async hydration committed only the new op, briefly erasing restored intent (crash window). | foundation-review.md:26-31; outbox.ts:207-231 |
| 4 | Hydration must yield to the UI (12-record batches) and stay fast. Restored-signature verification gates the first send. | Cold send fell from 347.9 ms to 181.4 ms after replacing nested timers. | foundation-review.md:32-39,66-70; outbox.ts:356-359 |
| 5 | Persist the unsigned intent before signing, and the signed bytes before publishing. **Never re-sign or re-date on retry.** Restored ops never auto-publish. | The acceptance journey is "signs once, persists signed event before publish, reload restores one unknown operation... no automatic sign/publish"; "Same signed event published again without re-signing". | hardening-validation.md:101-103,231-233; hardening-plan.md:89-91; outbox.ts:454-468 |
| 6 | Only proven non-delivery is `failed`. A transport error after the publisher was entered is `unknown`. A failed retry never downgrades `unknown`/`accepted`/`seen`. | Retrying an unknown op could otherwise produce a misleading "failed" and a duplicate send. | transport.ts:1312-1358; outbox.ts:400-406,507-517 |
| 7 | An event older than 15 min must be looked up by id (strong read) before re-publishing, because the relay rejects it on drift before dedupe. | Without the lookup, a successful earlier send looks like a rejection. | native.ts:835-858; buzz:crates/buzz-relay/src/handlers/ingest.rs:2449-2452 |
| 8 | A server duplicate reaction returns `accepted:false`, so a retry of a landed reaction becomes `failed` until the echo arrives. Plain duplicates return `accepted:true "duplicate:"`. | Observed in server code; not documented in the client. | buzz:crates/buzz-relay/src/handlers/ingest.rs:3288-3293, 3457-3461 |
| 9 | Revocation removes read visibility but **keeps pending/unknown ops with their exact ids and signed bytes**. Read visibility and durable recovery are separate concerns. | Hardening invariant; tested by revoke, regrant and restart with an unknown op. | hardening-plan.md:77-95; hardening-validation.md:101-103,232; outbox.ts:824-837 |
| 10 | A revoke during async hydration must fence the old confirmed cache loaded from disk. | Otherwise an old disk load undoes the revoke. | outbox.ts:335-344,824-837 |
| 11 | Authority comes from complete-roster and explicit denial. Partial discovery or a transport error is not removal. Newer live grants must survive older discovery work, and metadata failure must not delay roster authority. | Seven mutation defects: omitted session purge, omitted store call, immediate callbacks, late final-list commit, metadata-delayed authority, missing newer-grant protection, missing hydration fence. | hardening-plan.md:81-84,241-245; hardening-validation.md:77-82,229 |
| 12 | Purge denied data **before** notifying listeners, and fence late results with an access epoch. | Callbacks could re-seed denied data; held late HTTP completions must be ignored. | session.ts:385,409-415; hardening-validation.md:228 |
| 13 | Reference-only auxiliaries (reactions, deletes, edits, summaries) need known, visible targets. An `#h` tag cannot launder an unknown target. | Review found "generic 403, mixed-reference ... persisted-head callback gaps". | event-access.ts:3-44; hardening-validation.md:73-75 |
| 14 | A `void` API must not leak a Promise to UI lifecycle code. | Async `discover()` returned a Promise through `ensureList`, React used it as an effect cleanup, and the app got stuck on "Loading messages…". | hardening-plan.md:286-298; hardening-validation.md:154-166 |
| 15 | Scroll position must be saved from **current** metrics, not the previous frame. | A pre-existing bug saved bottom or zero, then faithfully restored the wrong value. | hardening-validation.md:243-257 |
| 16 | Persisted partition bytes `origin:viewer` must never change format. | "Changing these bytes orphans saved data that leaving a community can no longer purge." | partition.ts:1-3 |
| 17 | The heads cache keys `viewer:relay`, the reverse of every other store, and has a **global** cross-identity budget. A port that unifies keys must migrate or drop it. | Inconsistent scope order. | persistence.ts:43,181,187 |
| 18 | Multiple windows over one outbox partition are not coordinated. Each has its own in-memory journal. | An open issue in the review. | foundation-review.md:66-70 |
| 19 | Edits use second precision on every client. Successive own edits must be strictly increasing (`+1`), within 60 s of now and within the relay's ±900 s. | Protocol tie-break is by event id; local order would otherwise flip. | outbox.ts:686-709; message-order.ts:5-7 |
| 20 | Send ordering uses ms: own sends are strictly increasing per channel. Other authors' future timestamps can push ours at most 5 s ahead. A clock rollback past 60 s resets the chain. | Burst sends never tie or rewind. | message-order.ts:27-57 |
| 21 | The `recovery` key is unique among pending ops. Dismiss is blocked unless `failed`. Recovery is released only after its removal is durable. | Protects New-message drafts and member additions across crashes. | outbox.ts:240-271,597-605,795-807 |
| 22 | Workflow commands are never retried, only inspected or dismissed. They await a receipt even after the echo. | "inspect recent activity before submitting another command". | outbox.ts:116,505-521,771-776 |
| 23 | Guarded (invitation) ops need a live admission gate on retry, and the gate is checked again immediately before publish. | A cancelled channel addition must not publish. | outbox.ts:117-122,453,481,767-783 |
| 24 | Admission throttling (`ApiPaused`/`ApiNotSent`) means the request was not sent, which makes it a definitive failure. A lost response is never permission to repeat it. | | http-admission.ts:4-23; transport.ts:1224-1225 |
| 25 | A corrupt read-state journal is an error, never an empty journal to overwrite. | Prevents silently losing read intent. | read-state-storage.ts:59-60 |
| 26 | Plain sign-out leaves all per-viewer web storage on disk. Only erase wipes it. A sign-out that fails partway restores the moved folders and retries on the next launch. | A port must decide whether plain sign-out should purge the SQLite rows keyed by viewer. | sign_out.rs:1-8,546-608 |
| 27 | Leave-community purge is best effort and never retried. Failures are only reported. | The membership is already gone, so only a report can reach the viewer. | communities/device-state.ts:11-13 |

### 9.3 Reference-app quirks and protocol discrepancies: decide explicitly

These came out of comparing the client with the relay snapshot. Each one is a decision point.
Do not copy them by accident.

| # | Finding | Evidence | Recommendation |
|---|---|---|---|
| 1 | The relay rejects kind 5 and 9005 unless they carry **exactly one** `e`/`a` target. The app's `messages.remove()` emits up to 100 `e` tags, for example when it removes duplicate own reactions. | `buzz:crates/buzz-relay/src/handlers/ingest.rs:2908-2921`; `src/features/relay/messages.ts:262-311` | Emit one kind 5 per target, with `["h",ch],["e",id],["k",kind]`. |
| 2 | Voice-note `imeta` includes `waveform …`, but `waveform` is not in the relay's `ALLOWED_IMETA_KEYS`, so the message would be rejected with `invalid: disallowed imeta key`. | `buzz:crates/buzz-relay/src/handlers/imeta.rs:11-14`; `src/features/relay/attachments.ts:321-334` | Omit `waveform` unless the deployed relay accepts it. Keep reading it. |
| 3 | A duplicate reaction returns `200 {accepted:false,"duplicate: reaction already exists"}`. The client maps this to `failed` over HTTP and to `unknown` over WS, so a retried reaction that already landed shows as failed until its echo arrives. Plain duplicates return `accepted:true "duplicate:"`. | `buzz:crates/buzz-relay/src/handlers/ingest.rs:3284-3296,3457-3462`; `src/features/relay/transport.ts:1313-1359` | Treat `accepted:false` with a `duplicate:` prefix as accepted. |
| 4 | The live reconnect counter is only reset by a user Retry, never by a successful AUTH. Five drops over a socket's lifetime, even hours apart, leave live updates in `error`. | `src/features/relay/live.ts:223,671-677,1058` | Reset `attempts` after AUTH OK, or after the first EOSE. Cap backoff at 8 s and add jitter. |
| 5 | A presence `OK false` with no `retry in Ns` hint pauses **all** live admission (REQs and publications) for 86 401 s. | `src/features/relay/live.ts:761-776` | Pause presence only, and bound the pause, e.g. 60 s. |
| 6 | Nothing reconnects on network change or wake. The client never pings and does not handle NOTICE. The server closes unauthenticated sockets after 5 s and pings every 30 s, with 3 misses allowed. | `buzz:crates/buzz-relay/src/connection.rs:30,680-693,1070-1102` | Reconnect on OS network or wake events. Send WS pings with a ~45 s idle timeout. Answer server Pings. |
| 7 | A moderator 9005 is applied in the fold only if its signer is the target author. A live moderator delete leaves the row visible until a head reload replaces the window. | `src/features/relay/fold.ts:225-243`; `buzz:crates/buzz-relay/src/handlers/side_effects.rs:1806-1880` | Optionally apply a 9005 from a channel admin listed in 39001. If you keep parity, document the limitation. |
| 8 | Reconnect catch-up merges only a fresh 20-row head. If more than 20 top-level rows arrived while offline and older pages were loaded, a hole can remain. Live replay (`since=now-300`, ≤500) may hide it. | `src/features/relay/store.ts:2392-2431`; `docs/relay-queries.md:749-759` | Page the head forward until it overlaps retained rows, or mark a gap row. |
| 9 | Read-state **overrides** (`ov_s/ov_c/ov_b`) are parsed and merged but never created. Once any peer device publishes one, this device's publication is blocked ("Synchronized override publication is not enabled") and nothing clears it. | `src/features/relay/read-state.ts:531-533`; `src/features/relay/read-state-model.ts:204-223` | Port the parse and merge as-is. Decide whether to support override publication or ignore foreign overrides for publishing. |
| 10 | Unread counts are **sampled**: one 500-row read per 128 channels, then live evidence. A busy channel can starve its batch. The relay has an unused `GET/POST /buzz/v1/me/read-state` API, gated by NIP-11 `buzz_v1`. | `src/features/relay/unread.ts:1492-1556`; `buzz:crates/buzz-relay/src/api/buzz_v1/mod.rs:14-23` | Keep the semantics at first. Consider per-channel reads for starved channels later. |
| 11 | Mute affects notifications only, and only for DMs and thread replies. Mentions still notify, and mute never changes unread counts or the dock dot. Notifications also fire for kind 40008, which the docs omit. | `src/features/notifications/messages.ts:90-187`; `docs/unread.md:301` | Keep parity unless product decides otherwise. |
| 12 | Minted invites return `https://host/invite/<code>`, but the client only parses `buzz://join?relay=…&code=…`. | `src/features/communities/invite-link.ts:9-46`; `buzz:crates/buzz-relay/src/api/invites.rs:278-423` | Accept both forms. `https://host/invite/<code>` maps to (`https://host`, code). |
| 13 | The upload descriptor includes `dim`, `blurhash`, `thumb` and `duration`. The client drops them and never writes them to `imeta`. The relay allows `dim`, `blurhash` and `thumb`. | `src/features/relay/attachments.ts:55-97,284-334`; `buzz:crates/buzz-media/src/types.rs:7-31` | Parity first. Adding `dim` and `blurhash` is a cheap improvement, and other clients' fold already reads them. |
| 14 | Equal-second 39005 summaries tie-break to the **smaller id**, which can choose a stale count. | `src/features/relay/events.ts:141-149`; `src/features/relay/fold.ts:257-273` | Tie-break 39005 by the larger `descendant_count`, or accept the risk. |
| 15 | `consistency:"strong"` is valid only on plain (catch-all) filters. On a `thread_window` filter it causes a **400**. `top_level` and window filters ignore it. | `buzz:crates/buzz-core/src/thread_window.rs:85-98`; `buzz:crates/buzz-relay/src/api/bridge.rs:419-469` | Assert in the filter builder. |
| 16 | A 39006 `next_cursor` may point at an event that is **not** in the response. Echo it verbatim, and partition response events by kind, not by position. | `buzz:crates/buzz-relay/src/api/bridge.rs:683-865` | — |
| 17 | NIP-11 `self` is the relay's signing identity. NIP-11 `pubkey` is only the operator contact. | `src/features/relay/native.ts:324-333`; `buzz:crates/buzz-relay/src/nip11.rs:76-77` | — |
| 18 | Sidebar preference `d` tags (`channel-sections`, …) have no community scope. They are scoped only by being stored on that relay. | `src/features/relay/sidebar-preferences.ts:5-10` | Parity. Do not mirror them across communities. |
| 19 | The channel-heads cache keys use `viewer:relay` (reversed). Its 8 MiB budget is global across identities. | `src/features/relay/persistence.ts:43,181,187` | Use a single `(origin, viewer)` key order in SQLite. Do not migrate IndexedDB data. |
| 20 | Plain sign-out removes only the keychain entry. Per-viewer IndexedDB and localStorage data stay on disk. | `src-tauri/src/sign_out.rs:546-608` | Decide whether plain sign-out should delete the viewer's SQLite rows. |
| 21 | The app never sends `feed_types`, `resolve_thread_roots`, `["broadcast","1"]`, kind 20002 typing, 9005, 10030 or `client` tags (except `["client",uuid]` on 41010). Readers still handle `broadcast`. | grep; §4.2, §5d.2 | Parity. These are optional additions. |
| 22 | NIP-98 `payload` is required by the server in NIP-FI enforce mode and always for `/api/invites`. The client always sends `payload` on POST, plus a random `nonce`. The relay replay-guards on auth event ids, so a reused auth event gets a 401. | `buzz:crates/buzz-relay/src/api/bridge.rs:138-148,276-333`; `buzz:crates/buzz-auth/src/nip98.rs` | Always sign a fresh 27235 per request, including retries. |
