# Gateway keys for remote agents

A gateway drives your remote agents with its own key. Your main Nostr key stays
with you. Existing Buzz apps continue using your main key without a grant. Direct-message
sandboxes and unbound harness sessions never enable gateway access.

## Authority

An owner signs a kind `30181` addressable event naming one gateway. The
coordinate is `(30181, owner, gateway hex)`. Enabled heads require a NIP-40
`expiration` within 90 days. A newer disabled head revokes the grant permanently
until a newer enabled head replaces it.

```mermaid
sequenceDiagram
    participant Owner
    participant Relay
    participant Gateway
    participant Agent
    Owner->>Relay: Signed expiring grant naming gateway
    Gateway->>Relay: Signed thread prompt mentioning agent
    Agent->>Relay: Read current owner grant and gateway profile
    Agent->>Agent: Verify grant, membership and thread scope
    Agent->>Relay: Telemetry encrypted separately to owner and gateway
    Owner->>Relay: Newer disabled head
    Agent->>Relay: Refresh grant within 30 seconds
    Agent->>Agent: Refuse further gateway prompts and control
```

| Path | Authorization |
| --- | --- |
| Mention launch | Current signed owner grant, current remote-agent registration, visible trigger and channel |
| Thread prompt | Bound channel thread in strict owner mode; live grant checked at ingress and again before queued model dispatch |
| Turn cancellation | Live grant; signed, fresh NIP-44 observer control; command exactly `cancel_turn` |
| Observer telemetry | Agent ownership verified by relay; separate NIP-44 copy for each live gateway |
| Owner DMs, profile and channel administration | No delegated authority |
| Model changes and owner announcements | Owner key required |
| Explicit checkpoint/fresh recovery requests | Not delegated; existing agent-signed deployment remains required |

A gateway remains a separate ordinary relay member. A grant provides neither
relay membership nor channel membership. It does not impersonate the owner.
Normal actions permitted to that separate account remain available, including
posting its own records into an explicitly joined recording channel.

Only the newest signed head per coordinate counts. Consumers accept at most two
live gateways, ordered by grant timestamp and then gateway key. They read at most
32 heads per owner. Grants outside that window fail closed.

Agent keys never serve as gateways. The SDK refuses a gateway profile carrying
an `auth` tag. The relay also refuses keys recorded as owned agents. NIP-OA
owner attestations cannot substitute for gateway grants.

## Enroll and grant

The manager provisions a distinct gateway key in the per-developer daemon
Kubernetes Secret. Its enrollment reply returns only `gateway_pubkey`.
It never returns the gateway secret key. Provisioning and onboarding have
separate PRs; deploy those before using this enrollment path.

1. Enroll with the owner key using the Buzz app or `buzz onboard`.
2. Ask a relay operator to admit the returned gateway public key as a member.
3. Add that public key to each channel the daemon should use.
4. Run the grant command with your owner key.

```sh
# Relay operator, using the configured admin environment:
buzz-admin add-member --pubkey "$GATEWAY_PUBKEY" --role member

# Channel administrator:
buzz channels add-member --channel "$CHANNEL_ID" --pubkey "$GATEWAY_PUBKEY"

# Owner (BUZZ_PRIVATE_KEY is your owner key):
buzz gateway grant "$GATEWAY_PUBKEY" --expires 90d
buzz gateway list
buzz gateway revoke "$GATEWAY_PUBKEY"
```

The CLI refuses NIP-OA agent credentials for grant management. It reads the
current head before writing and advances its timestamp when two actions occur
within the same second. Conflicting writes fail visibly and can be retried.
`list` returns the latest 32 heads, including expired grants and revocations.

For key rotation, revoke the old public key first. An operator then deletes the
daemon Secret, and the owner enrolls again to generate new credentials. Admit
and grant the new public key. Remove the old key's relay and channel membership
when its records no longer need to remain accessible to that account.

## Revocation and failure behavior

Revocation uses a newer disabled replacement, without expiration. The relay
ignores NIP-09 deletion of gateway heads. Deleting a head could otherwise let an
old signed grant be replayed into an empty coordinate. Use `gateway revoke`.

Gateway authority also requires current posting membership in the bound channel.
The harness strongly reads the latest kind39002 roster signed by the current
NIP-11 relay key. The owner, agent, and gateway must all remain posting members.
A global grant never admits a gateway into a private channel. Membership removal
stops gateway prompts, cancellation, and telemetry after the 30-second cache
window. The launcher rechecks membership immediately before each launch.

For an explicit strong kind39002 lookup with the current relay author and one channel
coordinate, the relay checks canonical writer membership behind its existing
membership lock. A stale discovery projection is repaired before returning it.
Only a current channel member can trigger or receive that repair. Unchanged
rosters keep their event ID; this read path never broadcasts metadata.

Authority reads use `"consistency":"strong"` on HTTP queries and WebSocket REQ
filters. The relay pins these reads to the writer, so a lagging replica cannot
renew stale authority or hide the current revocation head.

The harness ages successful lookups from their start, for at most 30 seconds.
Each lookup has a five-second deadline. It serializes
refreshes so an older lookup cannot overwrite a newer result. A lookup failure
refuses gateway use and is retried after five seconds. Positive verdicts never
outlive the grant expiration. Owners remain usable during a gateway lookup
failure.

The relay caches observer verdicts for at most 30 seconds and fences local
writes by generation. Other replicas observe revocation within that cache TTL.
The mention launcher rechecks authority before each launch. New grants enter
its listener filters within the existing 60-second listener recycle interval.
Revocation does not cancel a model turn already running. The owner can cancel
that turn using the existing owner control path.

Telemetry fan-out is capped at two gateways. Additional recipients consume
subsequent idle ticks, preserving the existing average relay publication rate.
Ephemeral observer frames are not replayable history.

## Rollout and verification

Deploy the relay changes before granting gateway access. Then deploy the manager
and agent image containing the updated launcher and harness. Older harnesses
continue accepting owners but ignore gateways. No database migration is needed.

Exercise one owner agent in a test channel. Admit the gateway into that relay
and channel, grant it, and send a mention using the gateway key. Confirm one
sandbox starts and answers. Confirm the gateway receives encrypted observer
telemetry and can cancel a turn. Revoke it and wait 30 seconds. Confirm its next
prompt and cancellation are refused, while the owner's prompt still works.
Confirm a second owner's agent and the owner's DMs remain inaccessible through
the grant.
