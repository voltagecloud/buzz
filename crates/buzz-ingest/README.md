# buzz-ingest

`buzz-ingest` verifies GitHub, Linear, and public Slack webhooks, then publishes
ordinary thread messages to Buzz. It runs beside the relay, with its own member
key. The relay remains the durable store. Slack DMs and private channels are excluded.

| Source | Supported activity |
| --- | --- |
| GitHub | Pull requests, PR comments/reviews/review comments, and check runs. Checks target at most four linked PRs, or a commit thread. |
| Linear | Issues, labels added through issue updates, and comments. |
| Slack | Public-channel messages and message edits; signed URL verification. |

GitHub actions derive from authenticated body fields. The unsigned event header
must match the payload shape. Unsupported or unmapped activity returns an explicit
ignored response. Mapped publication returns 200 only after the relay accepts its
plans, anchor, and reply; incomplete writes return 503 for provider retry.

See [NIP-IG](../../docs/nips/NIP-IG.md) for the event and idempotency contract.
Single-replica Recreate deployment is required. No PVC or local journal is needed.

## Configuration

Set `BUZZ_INGEST_CONFIG` to this JSON file:

```json
{
  "relay_url": "https://buzz.voltage.cloud",
  "github_repos": {"voltagecloud/buzz": "<channel UUID>"},
  "linear_teams": {"<team UUID>": "<channel UUID>"},
  "linear_projects": {"<project UUID>": "<channel UUID>"},
  "linear_webhooks": {"<webhook UUID>": "<channel UUID>"},
  "linear_labels": {"<label UUID>": "agent"},
  "slack_channels": {"<Slack team ID>/<public channel ID>": "<channel UUID>"},
  "slack_workspace_url": "https://voltagecloud.slack.com"
}
```

Every mapping is optional; configure between one and 256 total entries.
Linear project mappings precede team mappings, then signed `webhookId` mappings.
A previously saved issue route is used only when none of those mappings applies.
Changing an explicit mapping redirects subsequent activity to the configured channel.
Use `linear_webhooks` when comments on existing issues omit project/team fields.
Previously mirrored issues also retain a relay-backed routing record.
No Linear API credential or data mutation is required.
[Linear issue payloads carry label UUIDs](https://github.com/linear/linear-solutions/blob/main/integration_guides/scenarios/03-webhooks/README.md).
Map names such as `agent` with `linear_labels`; unmapped labels retain their UUID.
Both issue creation and label additions populate `ext-added-labels`.

| Environment variable | Purpose |
| --- | --- |
| `BUZZ_INGEST_PRIVATE_KEY` | Required publisher Nostr secret key. |
| `BUZZ_INGEST_GITHUB_SECRET` | GitHub webhook HMAC secret; required with GitHub mappings. |
| `BUZZ_INGEST_LINEAR_SECRET` | Linear webhook HMAC secret; required with Linear mappings. |
| `BUZZ_INGEST_SLACK_SECRET` | Slack app signing secret; required with Slack mappings. |
| `BUZZ_INGEST_BIND_ADDR` | Listener address; default `0.0.0.0:5010`. |

Relay endpoints require HTTPS; loopback HTTP is allowed for isolated testing.
The process logs no provider payload, secret, or upstream error body.
It reads configuration and credentials at startup; restart after changes.

## Provider setup and rollout

1. Create the `buzz-ingest` ECR repository and grant the existing image publisher
   push access. CI publishes both native architectures from main; PRs build only.
2. Register the dedicated publisher as a relay member through `buzz-admin`.
   Add its public key to each mapped channel through channel administration.
3. Store the publisher key and selected provider secrets in a Kubernetes Secret.
4. Deploy the companion CD chart with one replica, Recreate, and the image digest.
5. Configure provider callbacks at `/ingest/github`, `/ingest/linear`, and
   `/ingest/slack` under the existing HTTPS hostname.
6. Create workflows with the publisher's public key in `trigger.authors`.

`/health` reports process health. `/ready` performs a signed strong relay query
and returns 503 when that query fails. Each relay request times out after two
seconds; the complete delivery has a four-second deadline. Provider retries
replay the stored plan. No background retry silently acknowledges failed work.

Bodies are limited to 1 MiB. Each provider admits 20 requests per second, and
one delivery publishes at a time. Up to 32 admitted requests can authenticate and
wait within the four-second deadline; excess requests receive 429. Message bodies retain
at most 6000 UTF-8 bytes. Payloads exceeding 16 labels are refused, so workflow metadata is never silently
truncated. Other metadata has separate bounds.

Configure GitHub org webhooks for the supported events and a shared secret.
[GitHub signs the raw body with HMAC-SHA256](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries).
Configure Linear issue/comment webhooks for mapped teams or webhook IDs.
[Linear signatures cover the raw body and its signed timestamp](https://linear.app/developers/webhooks);
requests outside five minutes are refused. Slack needs `message.channels` event
subscriptions, the `channels:history` bot scope, and membership in mapped public
channels. [Slack signs the version, timestamp, and raw body](https://docs.slack.dev/authentication/verifying-requests-from-slack/).
No DM/private-channel scopes are needed. GitHub does not automatically redeliver
failed webhooks; use its delivery history or an operator retry job to redeliver
failures. Linear and Slack retries are finite. Monitor failures and redeliver
after prolonged outages.

## Validation

```sh
. ./bin/activate-hermit
cargo test -p buzz-ingest
cargo clippy -p buzz-ingest --all-targets -- -D warnings
cargo run -p buzz-ingest
```

The tests drive the real HTTP router and NIP-98 client against a fake relay.
They exercise signature refusal, freshness, private Slack exclusion, write-ahead
ordering, lost acknowledgements, restart recovery, payload replay, stale pending
plans, strong-read failures, and mapping. External provider delivery and production
relay admission require deployment validation.
