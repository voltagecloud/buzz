# Local test relay

`scripts/test-relay.sh` runs a throwaway Buzz relay (upstream `block/buzz` at
`5142cf6b`) for client development. It does not touch other Buzz, postgres,
redis or minio instances on the machine.

```sh
scripts/test-relay.sh up       # build, start deps, migrate, start relay (background)
scripts/test-relay.sh seed     # members, profiles, channels, messages (runs once)
scripts/test-relay.sh verify   # WS NIP-42 + HTTP NIP-98 smoke checks
scripts/test-relay.sh status   # deps, relay pid, NIP-11, seeded channel ids
scripts/test-relay.sh logs     # tail .test-relay/relay.log (`logs deps` for docker)
scripts/test-relay.sh restart  # restart only the relay
scripts/test-relay.sh down     # stop relay + `docker compose -p buzz-gpui-test down` (data kept)
scripts/test-relay.sh wipe     # down + delete DB/minio data and seed.json
scripts/test-relay.sh up-tls   # also run a TLS relay at wss://localhost:53443
scripts/test-relay.sh down-tls # stop only the TLS relay and its TLS proxy
```

Fresh start: `wipe`, `up`, `seed`. All commands can be run again safely.
The first `up` builds with cargo in `nix shell` (cmake etc.) and gets `nak`
from nixpkgs.

State goes to `.test-relay/` next to the script. To use one test relay from
several git worktrees, set `TEST_RELAY_DIR` to the main checkout's
`.test-relay`.

## Endpoints and ports (all bound to 127.0.0.1)

| What | Address |
|------|---------|
| Relay WS | `ws://127.0.0.1:53000` |
| Relay HTTP (NIP-11, `POST /query`, `POST /events`, `/media`) | `http://127.0.0.1:53000` |
| Relay health / metrics | `:58080` / `:59102` |
| Postgres (`buzz`/`buzz_dev`, db `buzz`) | `:55432` |
| Redis | `:56379` |
| MinIO S3 / console (`buzz_dev`/`buzz_dev_secret`, bucket `buzz-media`) | `:59000` / `:59001` |
| TLS relay (`up-tls`): WS / HTTP | `wss://localhost:53443` / `https://localhost:53443` |
| TLS relay backend (plain; unusable directly) / health / metrics | `:53001` / `:58081` / `:59103` |

The deps run in compose project `buzz-gpui-test` (`scripts/test-relay.compose.yml`),
with containers named `buzz-gpui-test-*`. Typesense is not needed: this relay
version searches with Postgres full-text search.

## State (`.test-relay/`, gitignored)

- `buzz/`: upstream checkout and `target/debug/{buzz-relay,buzz-admin,buzz,buzz-test-cli}`
- `identities.json`: test keys (see below)
- `seed.json`: seeded channel UUIDs and event ids
- `relay.env`: stable relay signing key (`BUZZ_RELAY_PRIVATE_KEY`)
- `relay.log`, `relay.pid`, `build.log`, `data/` (postgres + minio), `git-repos/`
- `tls/`: the TLS relay (see below): `ca.pem` + `ca.key` (test CA),
  `server.pem` + `server.key` (localhost certificate), `ca-bundle.pem`,
  `relay.json`, `relay.log`/`relay.pid`, `proxy.log`/`proxy.pid`, `git-repos/`

`identities.json`:

```json
{
  "relay_ws_url": "ws://127.0.0.1:53000",
  "relay_http_url": "http://127.0.0.1:53000",
  "community_host": "127.0.0.1:53000",
  "identities": {
    "alice": {"role": "owner",  "display_name": "Alice", "sk_hex": "...", "nsec": "nsec1...", "pk_hex": "...", "npub": "npub1..."},
    "bob":   {"role": "member", ...},
    "carol": {"role": "member", ...}
  }
}
```

These keys are for testing only. Example:
`jq -r .identities.bob.nsec .test-relay/identities.json`.

## Seeded data

- Relay membership (NIP-43): alice is the owner (`RELAY_OWNER_PUBKEY`). bob and
  carol are members, added with `buzz-admin add-member`.
- Each identity has a kind 0 profile.
- Channels, all created by alice:
  - `#general`: open stream. alice, bob and carol are members.
  - `#random`: open stream. alice and bob are members.
  - `#core-team`: private stream. alice and bob only; carol must not see it.
  - `#announcements`: open forum.
  - An alice-bob DM.
- `#general` has 3 top-level kind 9 messages. alice's welcome message has 2
  thread replies (NIP-10 `["e",<root>,"","reply"]`) and 2 reactions (👍, 🎉).
  bob's message has a ❤️ reaction from alice.
- `#core-team` has a post and a thread reply. `#random` has 1 message.
- `#announcements` has a kind 45001 forum post and a kind 45003 comment.
- The DM has 2 kind 9 messages.

Seeding uses the upstream `buzz` CLI and `buzz-admin`. Channel discovery comes
from relay-signed kinds 39000, 39001 and 39002, with `d` set to the channel
UUID. Tags on 39000: `t` holds `stream`, `forum` or `dm`; `private` marks a
private channel; `hidden` marks a DM.

## Gotchas

- **The Host decides the community.** The relay resolves the community from
  the exact request authority. Only `127.0.0.1:53000` is mapped. With
  `localhost:53000`, the WS upgrade fails with 404 and HTTP returns `relay: no
  community is configured for this host`. NIP-11 still answers on any host.
- **Authentication is required.** NIP-11 advertises `auth_required: true`. The
  relay sends an AUTH challenge as soon as the socket connects. A REQ sent
  before AUTH gets `NOTICE auth-required: ...` and no events. Wait for the
  challenge, AUTH (kind 22242), then REQ.
- **Membership is enforced** (`BUZZ_REQUIRE_RELAY_MEMBERSHIP=true`). An
  authenticated key that is not a member reads nothing, and HTTP returns 403
  `relay_membership_required`. To admit another key, use `buzz-admin
  add-member`, or send a kind 9030 event signed by alice.
- **HTTP uses NIP-98 only** (`BUZZ_REQUIRE_AUTH_TOKEN=true`, so the dev
  `X-Pubkey` fallback is off). Send `Authorization: Nostr <base64 kind 27235>`
  with `u` set to the full URL and `method` set to the HTTP method.
  - The relay rejects a second identical auth event with `NIP-98: replay
    detected`, for example the same key, URL and method in the same second.
    Add a `nonce` tag (the upstream CLI uses a UUID) and a `payload` tag
    (sha256 of the body).
  - The body of `POST /query` is a JSON array of filters. The response is a
    JSON array of events.
- **Reactions have no `h` tag.** The relay stores a reaction in its target's
  channel and returns it for a `#h` filter. A client that re-checks events
  against its filter on the client side (nak does this) drops them. Do not
  filter `#h` on the client for kind 7.
- **Channel messages need an `h` tag.** kind 9 events without `h` are
  rejected. Threads use NIP-10 `e` tags with the `reply` marker.
- **DMs are channels.** DM messages are kind 9 in a hidden private channel.
  NIP-17 gift wraps (kind 1059) are also supported, but the seed does not use
  them.
- **Local patch to the upstream checkout.** Upstream binds the health and
  metrics listeners to `0.0.0.0`. `scripts/test-relay.sh` changes both to
  `127.0.0.1` with a `sed` before the build. Other upstream code is unchanged.
- **Disabled features.** The git object-store probe
  (`BUZZ_GIT_CONFORMANCE_PROBE=false`) and push are off.

## TLS relay and the agent end-to-end test

The agent controller accepts only `wss://` relays, so agents need a TLS relay.
`up-tls` runs one next to the plain relay and does not change it:

- A second `buzz-relay` process with `RELAY_URL=wss://localhost:53443`, its
  own database (`buzz_tls`, in the same postgres container) and Redis DB 1.
  It listens on `127.0.0.1:53001`. The relay takes the scheme of the
  expected NIP-42 `relay` tag and NIP-98 `u` URL from `RELAY_URL`, so one
  process cannot serve both `ws://` and `wss://`.
- `socat` (from nixpkgs) terminates TLS on `127.0.0.1:53443` and forwards the
  raw bytes, so the `Host` header (`localhost:53443`) and WebSocket upgrades
  pass through unchanged.
- A local CA (`tls/ca.pem`, generated once with `openssl`) signs a
  certificate for `localhost` and `127.0.0.1`. `tls/ca-bundle.pem` holds the
  system roots and the test CA. Point `SSL_CERT_FILE` at it: the app's
  HTTP and WebSocket clients (both rustls-platform-verifier), the `buzz`
  CLI and `buzz-acp` read it. `crates/relay/tests/tls_relay.rs` checks that
  live updates connect through it.
- `buzz-acp` is built from the same upstream checkout with
  `--features tokio-tungstenite/rustls-tls-native-roots`. Upstream's
  WebSocket client trusts only the bundled webpki roots, so without this
  feature it cannot trust the test CA. The source is not changed.
- `BUZZ_ALLOW_NIP_OA_AUTH=true`: agents are not relay members. The relay
  admits them through their owner's NIP-OA attestation.
- alice owns the community. `up-tls` creates an open `#general` as alice.
  bob and carol are not members. `tls/relay.json` records the URLs, the CA
  and bundle paths, the `buzz-acp` and `buzz` binaries, and the `#general`
  UUID.

Check it:

```sh
curl --cacert .test-relay/tls/ca.pem https://localhost:53443/_readiness
SSL_CERT_FILE=.test-relay/tls/ca-bundle.pem BUZZ_RELAY_URL=https://localhost:53443 \
  BUZZ_PRIVATE_KEY=$(jq -r .identities.alice.sk_hex .test-relay/identities.json) \
  .test-relay/buzz/target/debug/buzz channels list
```

`crates/host/tests/agent_e2e.rs` uses the TLS relay. It starts an
`AgentHost`, creates an agent as alice's, waits for the host to publish the
agent's profile, adds the agent to `#general`, and starts it. The agent's
harness is `crates/host/tests/fixtures/fake_acp_agent.py`, a fake ACP agent
with no model. The test mentions the agent in `#general` and waits for the
reply `echo: <mention>`, which the fake agent posts with the `buzz` CLI.
Then it stops and deletes the agent, and removes it from `#general`. The test
sets `HOME` to a temporary directory with `.local/bin/buzz-acp`, so the
controller launches the `buzz-acp` built by `up-tls`, not an installed one.
It also sets `SSL_CERT_FILE` itself.

```sh
scripts/test-relay.sh up-tls
BUZZ_TEST_RELAY_TLS=$PWD/.test-relay nix develop --command \
  cargo test -p buzz-host --test agent_e2e
```

Without `BUZZ_TEST_RELAY_TLS` the test is skipped. The test target has no
libtest harness (`harness = false`), because the controller starts the test
binary again as the agent supervisor.

**NixOS has no `/bin/ps`.** The agent controller lists an agent's processes
with `/bin/ps` to confirm that the agent stopped. Without it, Stop fails and
the supervisor stays parked, so the test refuses to run. Until the controller
is fixed, run the test binary in a user and mount namespace that provides
`/bin/ps`:

```sh
BIN=$(nix develop --command cargo test -p buzz-host --test agent_e2e --no-run \
  --message-format=json 2>/dev/null | jq -r 'select(.target.name == "agent_e2e") | .executable // empty')
BUZZ_TEST_RELAY_TLS=$PWD/.test-relay unshare -rm sh -c \
  'mount -t tmpfs tmpfs /bin && ln -s "$1" /bin/sh && ln -s "$2" /bin/ps && shift 2 && exec "$@"' \
  sh "$(readlink -f /bin/sh)" "$(command -v ps)" "$BIN"
```

## Using the upstream tools directly

```sh
B=.test-relay/buzz/target/debug
export BUZZ_RELAY_URL=http://127.0.0.1:53000
export BUZZ_PRIVATE_KEY=$(jq -r .identities.bob.sk_hex .test-relay/identities.json)
$B/buzz channels list
$B/buzz messages get --channel "$(jq -r .channels.general .test-relay/seed.json)"
nak req --fpa --auth --sec "$BUZZ_PRIVATE_KEY" -k 9 \
  -t h="$(jq -r .channels.general .test-relay/seed.json)" ws://127.0.0.1:53000 </dev/null
```

When you script `nak`, give it `</dev/null`. Without that, `nak req` tries to
read filters from stdin and can hang. Use `--fpa` so nak waits for the
challenge before it sends the REQ. `nak curl` signs with `NOSTR_SECRET_KEY`
and ignores `--sec`.
