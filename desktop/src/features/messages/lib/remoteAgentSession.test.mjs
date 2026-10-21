import assert from "node:assert/strict";
import { test } from "node:test";
import {
  publishedChannelMessage,
  publishedRemoteSession,
  remoteSessionOffersCheckpoint,
  remoteSessionRecoveryKind,
  usesThreadSandbox,
} from "./remoteAgentSession.ts";

test("published top-level and nested replies select their canonical workspace", () => {
  const channelId = "f9ee507e-d03c-4570-baa6-02951c38e1cd";
  const root = "a".repeat(64);
  const expected = { channelId, threadRoot: root };
  assert.deepEqual(
    publishedRemoteSession({ id: root, tags: [] }, channelId),
    expected,
  );
  assert.deepEqual(
    publishedRemoteSession(
      {
        id: "b".repeat(64),
        tags: [
          ["e", root.toUpperCase(), "", "root"],
          ["e", "c".repeat(64), "", "reply"],
        ],
      },
      channelId,
    ),
    expected,
  );
  assert.equal(publishedRemoteSession(undefined, channelId), undefined);
  assert.equal(publishedRemoteSession({ id: root, tags: [] }, null), undefined);
  assert.equal(
    publishedRemoteSession({ id: "invalid", tags: [] }, channelId),
    undefined,
  );
});

test("send results without relay tags still resolve their canonical workspace", () => {
  const channelId = "f9ee507e-d03c-4570-baa6-02951c38e1cd";
  const root = "a".repeat(64);
  const scope = (parentEventId, rootEventId) =>
    publishedRemoteSession(
      publishedChannelMessage(channelId, {
        eventId: "b".repeat(64),
        parentEventId,
        rootEventId,
      }),
      channelId,
    );
  assert.deepEqual(scope(null, null), {
    channelId,
    threadRoot: "b".repeat(64),
  });
  assert.deepEqual(scope(root, null), { channelId, threadRoot: root });
  assert.deepEqual(scope("c".repeat(64), root), {
    channelId,
    threadRoot: root,
  });
});

test("only an explicitly configured Kubernetes Sandbox uses thread launches", () => {
  assert.equal(
    usesThreadSandbox({
      backend: {
        type: "provider",
        id: "kubernetes",
        config: { sandbox: true },
      },
    }),
    true,
  );
  assert.equal(
    usesThreadSandbox({
      backend: { type: "provider", id: "kubernetes", config: {} },
    }),
    false,
  );
  assert.equal(
    usesThreadSandbox({
      backend: { type: "provider", id: "other", config: { sandbox: true } },
    }),
    false,
  );
  assert.equal(usesThreadSandbox({ backend: { type: "local" } }), false);
});

// Every lifecycle refusal the Kubernetes provider returns (sandbox.rs), plus
// errors that must not offer recovery.
const providerErrors = [
  [
    "Sandbox session ended (stopped); explicit recovery is required",
    "recover",
    true,
  ],
  ["Sandbox session ended; explicit recovery is required", "recover", true],
  [
    "Sandbox Pod was replaced; credentials remain bound to the original session, explicit recovery is required",
    "recover",
    true,
  ],
  [
    "Sandbox is not an active managed session; explicit recovery is required",
    "recover",
    true,
  ],
  [
    "Sandbox session is ending; recover it explicitly once it has ended",
    "ending",
    true,
  ],
  [
    "this session has no verified checkpoint; start fresh instead",
    "recover",
    false,
  ],
  [
    "no ended session exists for this thread to recover; start fresh instead",
    "recover",
    false,
  ],
  [
    "recovery requires an ended session; this Sandbox is still active, so end it first",
    "end-first",
    true,
  ],
  [
    "Sandbox launch configuration differs; end the session, then explicit recovery is required",
    "end-first",
    true,
  ],
  [
    "Sandbox belongs to a different relay; use a new thread or explicit recovery",
    null,
    true,
  ],
  [
    "agent already has a Pod; stop and recover it explicitly before using Sandbox",
    null,
    true,
  ],
  ["provider timed out after 600s", null, true],
  ["", null, true],
];

test("provider lifecycle errors map to the explicit action they ask for", () => {
  for (const [error, kind, offersCheckpoint] of providerErrors) {
    assert.equal(remoteSessionRecoveryKind(error), kind, error);
    assert.equal(
      remoteSessionRecoveryKind(error.toUpperCase()),
      kind,
      `case-insensitive: ${error}`,
    );
    assert.equal(remoteSessionOffersCheckpoint(error), offersCheckpoint, error);
  }
});

test("provider codes override prose and survive Tauri error wrapping", () => {
  for (const [code, kind, checkpoint] of [
    ["session_active", "end-first", true],
    ["config_differs", "end-first", true],
    ["session_ending", "ending", true],
    ["session_ended", "recover", true],
    ["no_checkpoint", "recover", false],
    ["no_ended_session", "recover", false],
    ["future_refusal", null, true],
  ]) {
    const payload = { code, message: "session ended; start fresh instead" };
    for (const error of [
      payload,
      Object.assign(new Error(payload.message), { payload }),
    ]) {
      assert.equal(remoteSessionRecoveryKind(error), kind, code);
      assert.equal(remoteSessionOffersCheckpoint(error), checkpoint, code);
    }
  }
});
