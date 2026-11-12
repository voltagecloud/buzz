import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile, chmod, open } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import path from "node:path";
import os from "node:os";
import contribute, { createRecorder, scrub } from "./recorder/server/recorder.mjs";
const root = await mkdtemp(path.join(os.tmpdir(), "buzz-recorder-test-"));
const spool = path.join(root, "spool");
const channel = "6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e";
const hex = (value) => createHash("sha256").update(value).digest("hex");
const payload = { agent: { id: "agent-1", provider: "claude" }, turnId: "turn-1", outcome: { kind: "completed" } };
const entries = [
  { seqStart: 1, seqEnd: 1, turnId: "turn-1", item: { type: "user_message", text: "Please ask @brokkr" } },
  { seqStart: 2, seqEnd: 2, turnId: "turn-1", item: { type: "tool_call", name: "SECRET_TOOL", status: "completed", output: "NEVER COPY OUTPUT" } },
  { seqStart: 3, seqEnd: 3, turnId: "turn-1", item: { type: "assistant_message", text: "My first answer" } },
  { seqStart: 4, seqEnd: 4, turnId: "turn-1", item: { type: "assistant_message", text: "My second answer" } },
  { seqStart: 5, seqEnd: 5, turnId: "turn-2", item: { type: "assistant_message", text: "OTHER TURN" } },
];
let available = true;
let timeline = entries;
let epoch = "epoch-1";
const context = { paseo: { agents: { ref: () => ({ timeline: { refetch: async (options) => {
  assert.equal(options.timeout, 5000);
  if (!available) throw new Error("offline");
  return { epoch, entries: timeline, hasOlder: false };
} } }) } } };
const options = { directory: spool, channel, retryMs: 60000 };
const active = async () => (await readdir(spool)).filter((name) => !name.includes(".state."));
const reset = async () => { await rm(spool, { recursive: true, force: true }); await mkdir(spool); };
const prepared = async (index, content) => {
  const id = index.toString(16).padStart(64, "0");
  await writeFile(path.join(spool, `${id}.json`), JSON.stringify({ version: 2, id, session: id, agentId: `agent-${index}`, turnId: "turn", order: index, next: 0,
    messages: [{ content, seqEnd: 1, role: "developer", part: 1, parts: 1, entry: id,
      metadata: { session: id, entry: id, role: "developer", part: 1, parts: 1, sequence: 1, created_at: 100 } }] }));
};
const ack = async (content, _channel, { metadata }) => hex(JSON.stringify(metadata) + content);
let recorder;
try {
  await reset();
  for (const value of ["ghp_abc123", "sk-ant-token", "password=secret", "nsec1abc", "at-session", "eyJh.e30.signature"]) assert.match(scrub(value, {}), /omitted/);
  assert.match(scrub(Buffer.alloc(32, 0x87).toString("base64"), {}), /omitted/, "opaque credentials remain scrubbed");
  for (const label of ["private_key", "privateKey", "secret", "access_token", "apiKey"]) {
    assert.match(scrub(JSON.stringify({ [label]: "d".repeat(64) }), {}), /omitted/, "quoted credential labels remain sensitive");
  }
  const publicFields = JSON.stringify({ pubkey: "d".repeat(64), event_id: "a".repeat(64) });
  assert.equal(scrub(publicFields, {}), publicFields);
  // Ordinary answers about code survive: commit SHAs, paths, and prose that
  // mentions tokens without a key=value or quoted-key credential.
  for (const prose of [
    "The fix is in commit 4dcd813471690fd39b9b0a2e6ac0faf3cf3e8f82.",
    "See voltage/images/paseo-daemon/recorder/server/recorder.mjs line 30.",
    "Look at crates/buzz-cli/src/commands/messages/transcript.rs",
    "Use the token: the gateway key is granted",
    "The OAuth token = refreshed each hour",
  ]) assert.equal(scrub(prose, {}), prose, prose);
  const publicHex = "a1".repeat(32);
  assert.equal(scrub(`event ${publicHex} pubkey ${"f0".repeat(32)}`, {}), `event ${publicHex} pubkey ${"f0".repeat(32)}`);
  assert.match(scrub(publicHex, { BUZZ_PRIVATE_KEY: publicHex }), /omitted/);
  const hooks = new Map(); process.env.BUZZ_PASEO_RECORD_CHANNEL = channel; process.env.PASEO_HOME = root;
  const dispose = contribute({ addHook(name, handler) { hooks.set(name, handler); return () => {}; } });
  const request = { agentId: "synthetic", provider: "claude" };
  assert.equal(hooks.get("agent.open.before")({ request }, context), request, "before-hook returns the request, not its envelope");
  assert.ok(hooks.has("agent.turn.before") && hooks.has("agent.turn.after"));
  await dispose(); delete process.env.BUZZ_PASEO_RECORD_CHANNEL;

  const sent = [];
  recorder = createRecorder({ ...options, send: async (...args) => { sent.push({ content: args[0], metadata: structuredClone(args[2].metadata) }); return ack(...args); } });
  await recorder.capture({ ...payload, agent: { ...payload.agent, provider: "codex" } }, context);
  assert.equal((await active()).length, 0);
  available = false; await recorder.capture(payload, context); await recorder.flush();
  assert.equal((await active()).length, 1, "timeline failure retains intent");
  available = true; await recorder.flush();
  assert.deepEqual(sent.map((message) => message.content), ["Please ask @brokkr", "My first answer", "My second answer"]);
  assert.deepEqual(sent.map((message) => message.metadata.role), ["developer", "assistant", "assistant"]);
  assert.ok(sent.every((message) => !/SECRET_TOOL|NEVER COPY OUTPUT|OTHER TURN/.test(JSON.stringify(message))));
  assert.equal(sent[0].metadata.root, undefined);
  const rootId = await ack(sent[0].content, channel, { metadata: sent[0].metadata });
  assert.ok(sent.slice(1).every((message) => message.metadata.root === rootId));
  assert.ok(sent[0].metadata.created_at < sent[1].metadata.created_at && sent[1].metadata.created_at < sent[2].metadata.created_at);
  await recorder.capture(payload, context); await recorder.flush();
  assert.equal(sent.length, 3, "duplicate hook cannot duplicate accepted entries");
  await recorder.close();

  // A new process continues the same thread, and long multilingual text
  // reassembles byte-for-byte rather than being truncated.
  const long = "你好world".repeat(1000);
  timeline = [{ seqStart: 6, seqEnd: 6, turnId: "turn-2", item: { type: "assistant_message", text: long } }];
  const split = [];
  recorder = createRecorder({ ...options, send: async (...args) => { split.push({ content: args[0], metadata: structuredClone(args[2].metadata) }); return ack(...args); } });
  await recorder.capture({ ...payload, turnId: "turn-2" }, context); await recorder.flush(); await recorder.close();
  assert.equal(split.map((message) => message.content).join(""), long);
  assert.ok(split.length > 1 && split.every((message) => Buffer.byteLength(message.content) <= 6000 && !message.content.includes("\ufffd")));
  assert.ok(split.every((message) => message.metadata.root === rootId));

  // Replacement epochs can reset sequence numbers; the old cursor cannot
  // silently discard new text from the same native session.
  epoch = "epoch-2"; timeline = [{ seqStart: 1, seqEnd: 1, turnId: "turn-3", item: { type: "assistant_message", text: "after replacement" } }];
  const replaced = [];
  recorder = createRecorder({ ...options, send: async (...args) => { replaced.push(args[0]); return ack(...args); } });
  await recorder.capture({ ...payload, turnId: "turn-3" }, context); await recorder.flush(); await recorder.close();
  assert.deepEqual(replaced, ["after replacement"]); epoch = "epoch-1";

  // Oversize input is refused without storing a giant prepared job or
  // truncating a successful message; failure counts can still advance.
  await reset(); timeline = [{ seqStart: 1, seqEnd: 1, turnId: "turn-1", item: { type: "assistant_message", text: "x".repeat(1024 * 1024 + 1) } }];
  recorder = createRecorder({ ...options, send: ack }); await recorder.capture(payload, context); await recorder.flush();
  const oversized = JSON.parse(await readFile(path.join(spool, (await active())[0])));
  assert.equal(oversized.attempts, 1); assert.equal(oversized.messages, undefined);
  await recorder.close();

  // A developer can queue the next prompt between two assistant messages.
  // Record that prompt at its real position, once, without skipping either
  // assistant message or publishing an unfinished later-turn fragment.
  await reset();
  timeline = [
    { seqStart: 1, seqEnd: 1, turnId: "turn-1", item: { type: "user_message", text: "first prompt" } },
    { seqStart: 2, seqEnd: 2, turnId: "turn-1", item: { type: "assistant_message", text: "first answer" } },
    { seqStart: 3, seqEnd: 3, turnId: "turn-2", item: { type: "user_message", text: "queued prompt" } },
    { seqStart: 4, seqEnd: 4, turnId: "turn-1", item: { type: "assistant_message", text: "first final" } },
    { seqStart: 5, seqEnd: 5, turnId: "turn-2", item: { type: "assistant_message", text: "second answer" } },
  ];
  const queuedText = [];
  recorder = createRecorder({ ...options, send: async (...args) => { queuedText.push(args[0]); return ack(...args); } });
  await recorder.capture(payload, context); await recorder.flush();
  assert.deepEqual(queuedText, ["first prompt", "first answer", "queued prompt", "first final"]);
  await recorder.capture({ ...payload, turnId: "turn-2" }, context); await recorder.flush(); await recorder.close();
  assert.deepEqual(queuedText, ["first prompt", "first answer", "queued prompt", "first final", "second answer"]);

  // Stop accumulating text before another page can exceed the retained
  // budget. Each page individually fits; a third page must not be requested.
  await reset(); let pageCalls = 0;
  const pageText = "你好".repeat(100000);
  const pages = { paseo: { agents: { ref: () => ({ timeline: { refetch: async () => {
    pageCalls++;
    if (pageCalls > 2) throw new Error("unexpected third page");
    const seq = 3 - pageCalls;
    return { epoch: "paged", entries: [{ seqStart: seq, seqEnd: seq, turnId: "turn-1", item: { type: "user_message", text: pageText } }], hasOlder: true, startCursor: { epoch: "paged", seq } };
  } } }) } } };
  recorder = createRecorder({ ...options, shutdownMs: 1, send: ack });
  await recorder.capture(payload, pages); await recorder.flush();
  assert.equal(pageCalls, 2, "retained-byte admission must stop pagination before further allocation");
  assert.equal(JSON.parse(await readFile(path.join(spool, (await active())[0]))).messages, undefined);
  await recorder.close();

  // Hold the real state read across shutdown. A timeline rejection created
  // after the deadline must still be observed, not crash the plugin.
  await reset();
  const fifo = path.join(spool, `${hex(payload.agent.id)}.state.json`);
  execFileSync("mkfifo", [fifo]);
  const unhandled = [];
  const rejection = (error) => unhandled.push(error);
  process.on("unhandledRejection", rejection);
  try {
    let queried = false;
    const rejecting = { paseo: { agents: { ref: () => ({ timeline: { refetch: async () => {
      queried = true; throw new Error("timeline rejected after deadline");
    } } }) } } };
    recorder = createRecorder({ ...options, shutdownMs: 1, send: ack });
    await recorder.capture(payload, rejecting);
    const inFlight = recorder.flush();
    const writer = await open(fifo, "w"); // Reader is now inside the state-file await.
    const closing = recorder.close();
    await new Promise((resolve) => setTimeout(resolve, 20));
    await writer.writeFile(JSON.stringify({ root: null, lastSeq: 0, sequence: 0, createdAt: 0 }));
    await writer.close();
    await inFlight; await closing;
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(queried, true, "fixture must reach the rejecting production timeline operation");
    assert.deepEqual(unhandled, [], "expired wait must consume the in-flight rejection");
    assert.equal((await active()).length, 1, "shutdown keeps the unacknowledged intent");
  } finally { process.removeListener("unhandledRejection", rejection); }

  // Ambiguous acknowledgement retries the frozen metadata/content exactly.
  await reset(); await prepared(1, "retry literal");
  const attempts = [];
  recorder = createRecorder({ ...options, send: async (content, _channel, { metadata }) => { attempts.push(JSON.stringify({ content, metadata })); throw new Error("lost ACK"); } });
  await recorder.flush(); await recorder.close();
  recorder = createRecorder({ ...options, send: async (...args) => { attempts.push(JSON.stringify({ content: args[0], metadata: args[2].metadata })); return ack(...args); } });
  await recorder.flush(); await recorder.close();
  assert.ok(attempts.length >= 2 && attempts.every((attempt) => attempt === attempts[0]));
  assert.equal((await active()).length, 0);

  // Eight failing sessions cannot starve the ninth. Failures keep retrying for
  // the whole retry window, so a relay outage loses nothing; only after the
  // window do they dead-letter.
  await reset(); for (let index = 0; index < 9; index++) await prepared(index, index < 8 ? "bad" : "healthy");
  const rotated = [];
  const deliver = async (...args) => { if (args[0] === "bad") throw new Error("permanent refusal"); rotated.push(args[0]); return ack(...args); };
  recorder = createRecorder({ ...options, send: deliver }); await recorder.flush(); await recorder.flush();
  assert.deepEqual(rotated, ["healthy"]); await recorder.close();
  recorder = createRecorder({ ...options, send: deliver }); for (let batch = 0; batch < 6; batch++) await recorder.flush(); await recorder.close();
  assert.equal((await active()).filter((name) => name.endsWith(".dead.json")).length, 0, "an outage inside the window dead-letters nothing");
  recorder = createRecorder({ ...options, send: deliver, retryWindowMs: 0 }); await recorder.flush(); await recorder.close();
  assert.equal((await active()).filter((name) => name.endsWith(".dead.json")).length, 8);
  for (const name of await active()) assert.equal(JSON.parse(await readFile(path.join(spool, name))).attempts, 5);
  // Dead letters are capped and never count toward admission.
  for (let index = 100; index < 140; index++) {
    await prepared(index, "bad");
    const id = index.toString(16).padStart(64, "0");
    const job = JSON.parse(await readFile(path.join(spool, `${id}.json`)));
    await writeFile(path.join(spool, `${id}.json`), JSON.stringify({ ...job, attempts: 4, failingSince: 0 }));
  }
  recorder = createRecorder({ ...options, send: deliver, retryWindowMs: 0 });
  for (let batch = 0; batch < 6; batch++) await recorder.flush();
  await recorder.close();
  assert.equal((await active()).filter((name) => name.endsWith(".dead.json")).length, 32, "dead letters are capped");
  // A full set of idle session cursors does not refuse a new session.
  await reset();
  for (let index = 0; index < 256; index++) {
    await writeFile(path.join(spool, `${index.toString(16).padStart(64, "0")}.state.json`), JSON.stringify({ root: null, lastSeq: 0, sequence: 0, createdAt: 0 }));
  }
  recorder = createRecorder({ ...options, send: ack });
  await recorder.capture(payload, context);
  await recorder.close();
  assert.equal((await active()).filter((name) => name.endsWith(".state.json")).length <= 256, true);
  assert.ok((await active()).some((name) => !name.includes(".state.")), "the new session's turn was admitted");

  // Admission caps apply before a blocked timeline can retain hook payloads.
  await reset(); timeline = entries;
  let release; let started;
  const blocked = new Promise((resolve) => { release = resolve; });
  const began = new Promise((resolve) => { started = resolve; });
  let first = true;
  const slow = { paseo: { agents: { ref: () => ({ timeline: { refetch: async () => { if (first) { first = false; started(); await blocked; } return { epoch: "epoch-1", entries: [], hasOlder: false }; } } }) } } };
  recorder = createRecorder({ ...options, send: ack }); await recorder.capture(payload, slow);
  const flushing = recorder.flush(); await began;
  const queued = Array.from({ length: 256 }, (_, index) => recorder.capture({ ...payload, turnId: `queued-${index}` }, slow));
  await assert.rejects(recorder.capture({ ...payload, turnId: "overflow" }, slow), /queue full/);
  release(); await flushing; await Promise.all(queued);
  assert.ok((await active()).length <= 256);
  await recorder.close();
  await reset(); for (let index = 0; index < 256; index++) await prepared(index, "existing");
  recorder = createRecorder({ ...options, shutdownMs: 1, send: ack });
  await assert.rejects(recorder.capture({ ...payload, turnId: "disk-full" }, slow), /spool full/);
  await recorder.close();

  // Final sweep flushes, but an unresponsive publisher cannot exceed grace.
  await reset(); await prepared(1, "shutdown"); let shutdownSent = 0;
  recorder = createRecorder({ ...options, send: async (...args) => { shutdownSent++; return ack(...args); } });
  await recorder.close(); assert.equal(shutdownSent, 1); assert.equal((await active()).length, 0);
  await prepared(2, "blocked");
  recorder = createRecorder({ ...options, shutdownMs: 50, send: () => new Promise(() => {}) });
  const start = Date.now(); const flush = recorder.flush(); await recorder.close(); await flush;
  assert.ok(Date.now() - start < 1000); assert.equal((await active()).length, 1);
  // The real child publisher is killed as a process group at shutdown.
  await recorder.close(); await reset(); await prepared(3, "child blocked");
  const oldPath = process.env.PATH;
  process.env.PATH = `${root}:${oldPath}`; process.env.PUBLISH_PID_FILE = path.join(root, "publisher-pids");
  await writeFile(path.join(root, "buzz"), '#!/bin/sh\nsleep 30 &\nprintf "%s %s" "$$" "$!" >"$PUBLISH_PID_FILE"\nwait\n');
  await chmod(path.join(root, "buzz"), 0o755);
  recorder = createRecorder({ ...options, shutdownMs: 50 });
  const childFlush = recorder.flush();
  for (let retry = 0; retry < 50; retry++) {
    try { if (/^\d+ \d+$/.test(await readFile(process.env.PUBLISH_PID_FILE, "utf8"))) break; } catch {}
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  const pids = (await readFile(process.env.PUBLISH_PID_FILE, "utf8")).split(" ").map(Number);
  assert.ok(pids.length === 2 && pids.every((pid) => Number.isSafeInteger(pid) && pid > 1));
  await recorder.close(); await childFlush; await new Promise((resolve) => setTimeout(resolve, 50));
  try {
    for (const pid of pids) {
      try { assert.equal((await readFile(`/proc/${pid}/stat`, "utf8")).split(") ")[1][0], "Z", "publisher descendant must have stopped"); }
      catch (error) { if (error.code !== "ENOENT") throw error; }
    }
  } finally {
    try { process.kill(-pids[0], "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    process.env.PATH = oldPath; delete process.env.PUBLISH_PID_FILE;
  }
  console.log("PASS: full ordered transcript, no tools, role tags, flat root, UTF8 splitting, restart, stable retries, dead letters, admission and shutdown");
} finally { await recorder?.close(); await rm(root, { recursive: true, force: true }); }
