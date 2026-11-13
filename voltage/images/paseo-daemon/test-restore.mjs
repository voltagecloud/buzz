// Conversation restore against a fake ACP bridge (stdio JSON-RPC) and a fake
// `paseo` CLI, both resolved from PATH like the real ones.
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { buzzProviders } from "./providers.mjs";
import { LIMITS, createRestorer, failures, restore, restorePass, run } from "./restore.mjs";

const directory = await realpath(await mkdtemp(path.join(os.tmpdir(), "paseo-restore-test-")));
const bin = path.join(directory, "bin");
const fixture = path.join(directory, "fixture.json");
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };

// The bridge serves the fixture entry of its --agent: "list" pages sessions
// (an error from page `failFrom` on; `startsConversation` writes a Paseo
// record for that provider while listing), "nolist" lacks session listing,
// "hang" never answers (and leaves a grandchild), "flood" writes an endless line.
const bridge = `#!/usr/bin/env node
const fs = require("node:fs"); const path = require("node:path"); const { spawn } = require("node:child_process");
const agent = process.argv[process.argv.indexOf("--agent") + 1];
const spec = JSON.parse(fs.readFileSync(process.env.RESTORE_FIXTURE, "utf8"))[agent];
fs.appendFileSync(process.env.RESTORE_FIXTURE + ".spawns", agent + "\\n");
const record = { argv: process.argv.slice(2), password: process.env.PASEO_PASSWORD, key: process.env.BUZZ_PRIVATE_KEY, lists: 0 };
const save = () => fs.writeFileSync(process.env.RESTORE_FIXTURE + "." + agent, JSON.stringify(record));
save();
if (spec.mode === "hang") {
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1e9)"], { stdio: "ignore" });
  fs.writeFileSync(process.env.RESTORE_FIXTURE + ".grandchild", String(child.pid));
  setInterval(() => {}, 1e9);
} else if (spec.mode === "flood") {
  const chunk = "x".repeat(65536); const write = () => { while (process.stdout.write(chunk)); process.stdout.once("drain", write); }; write();
} else {
  let buffer = "";
  process.stdin.on("data", (data) => {
    buffer += data;
    for (let at = buffer.indexOf("\\n"); at >= 0; at = buffer.indexOf("\\n")) {
      const message = JSON.parse(buffer.slice(0, at)); buffer = buffer.slice(at + 1);
      let result;
      let error;
      if (message.method === "initialize") result = { protocolVersion: 1, agentCapabilities: { loadSession: true, ...(spec.mode === "list" ? { sessionCapabilities: { list: {} } } : {}) } };
      if (message.method === "session/list") {
        record.lists += 1; save();
        if (spec.startsConversation) {
          const records = path.join(process.env.PASEO_HOME, "agents", "live");
          fs.mkdirSync(records, { recursive: true });
          fs.writeFileSync(path.join(records, "new.json"), JSON.stringify({ provider: spec.startsConversation, persistence: { provider: spec.startsConversation, sessionId: "0b8f2c1e-1f7a-4c55-9d0e-3a2b1c0d9e8f" } }));
        }
        const offset = Number(message.params.cursor ?? 0);
        const page = spec.sessions.slice(offset, offset + spec.pageSize);
        if (spec.failFrom !== undefined && offset >= spec.failFrom) error = { code: -32603, message: "relay unavailable" };
        else result = { sessions: page, ...(offset + spec.pageSize < spec.sessions.length ? { nextCursor: String(offset + spec.pageSize) } : {}) };
      }
      process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: message.id, ...(error ? { error } : { result }) }) + "\\n");
    }
  });
}
`;
// \`paseo project ls --json\` lists the fixture's project unless the daemon
// is down. \`paseo import --provider <id> --cwd <dir> -- <session>\` records
// the call and writes an agent record like the daemon's; listed ids fail.
const paseo = `#!/usr/bin/env node
const fs = require("node:fs"); const path = require("node:path");
const args = process.argv.slice(2);
const spec = JSON.parse(fs.readFileSync(process.env.RESTORE_FIXTURE, "utf8"));
if (args.join(" ") === "project ls --json") {
  if (spec.daemonDown) process.exit(1);
  process.stdout.write(JSON.stringify([{ path: "/" }, { path: spec.project }]));
  process.exit(0);
}
const option = (name) => args[args.indexOf(name) + 1];
if (args[0] !== "import" || args[args.length - 2] !== "--") process.exit(2);
const call = { provider: option("--provider"), cwd: option("--cwd"), sessionId: args[args.length - 1], password: process.env.PASEO_PASSWORD };
fs.appendFileSync(process.env.RESTORE_FIXTURE + ".imports", JSON.stringify(call) + "\\n");
if (spec.failImports.includes(call.sessionId)) process.exit(1);
const records = path.join(process.env.PASEO_HOME, "agents", "imported");
fs.mkdirSync(records, { recursive: true });
fs.writeFileSync(path.join(records, call.provider + "-" + call.sessionId + ".json"), JSON.stringify({ id: call.sessionId, provider: call.provider, persistence: { provider: call.provider, sessionId: call.sessionId } }));
`;

const owner = "b".repeat(64);
const brokkr = { id: "brokkr", label: "Brokkr", agent: "a".repeat(64), channel: "6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e" };
const tyr = { ...brokkr, id: "tyr", label: "Tyr", agent: "c".repeat(64) };
const now = Date.now();
const hours = (count) => new Date(now - count * 3_600_000).toISOString();
const id = (index) => index.toString(16).padStart(64, "0");
const listing = (count) => Array.from({ length: count }, (_, index) => ({ sessionId: id(index), cwd: "/", title: `t${index}`, updatedAt: hours(index) }));

/** A fresh $HOME with ~/buzz, a project directory, and an empty agent store. */
async function makeHome(name, agents = []) {
  const home = path.join(directory, name);
  const paseoHome = path.join(home, ".paseo");
  await mkdir(path.join(home, "code", "voltagecloud", "backend"), { recursive: true });
  await mkdir(path.join(home, "buzz"));
  await mkdir(path.join(paseoHome, "agents", "project"), { recursive: true });
  await writeFile(path.join(paseoHome, "config.json"), JSON.stringify({ agents: { providers: {
    claude: { enabled: true, command: ["claude"] }, ...buzzProviders(agents, owner, ["claude"]),
  } } }));
  const env = { ...process.env, HOME: home, PASEO_HOME: paseoHome, RESTORE_FIXTURE: fixture,
    PASEO_PASSWORD: "paseo-password-secret", BUZZ_PRIVATE_KEY: "1".padStart(64, "0") };
  return { home, paseoHome, env, configFile: path.join(paseoHome, "config.json") };
}
const writeRecord = (paseoHome, name, fields) => writeFile(path.join(paseoHome, "agents", "project", `${name}.json`), JSON.stringify(fields));
const setFixture = async (home, specs) => {
  await rm(`${fixture}.imports`, { force: true });
  await rm(`${fixture}.spawns`, { force: true });
  await writeFile(fixture, JSON.stringify({ failImports: [], project: path.join(home, "buzz"), ...specs }));
};
const lines = async (file) => (await readFile(file, "utf8").catch(() => "")).trim().split("\n").filter(Boolean);
const imports = async () => (await lines(`${fixture}.imports`)).map((line) => JSON.parse(line));
const spawns = async () => (await lines(`${fixture}.spawns`)).length;
const cli = (env) => spawnSync(process.execPath, [fileURLToPath(new URL("restore.mjs", import.meta.url))], { env, encoding: "utf8", timeout: 30_000 });
const configured = (agent, providerId, env) => ({ id: providerId, command: ["buzz", "acp-bridge", "--agent", agent], env });

try {
  await mkdir(bin);
  await writeFile(path.join(bin, "buzz"), bridge);
  await writeFile(path.join(bin, "paseo"), paseo);
  await chmod(path.join(bin, "buzz"), 0o755);
  await chmod(path.join(bin, "paseo"), 0o755);
  // In-process runs resolve `paseo` and read the fixture like the CLI does.
  process.env.PATH = `${bin}:${process.env.PATH}`;
  process.env.RESTORE_FIXTURE = fixture;

  // A fresh home: the 20 newest listed sessions, minus the unreadable id, are
  // imported newest first; listing stops at the cap. A native agent's record
  // neither makes the Buzz provider stale nor counts against the cap.
  const a = await makeHome("a", [brokkr, tyr]);
  await writeRecord(a.paseoHome, "pa", { id: "pa", provider: "claude", persistence: { provider: "claude", sessionId: "native" } });
  await symlink(path.join(a.home, "code", "voltagecloud", "backend"), path.join(a.home, "code", "link"));
  // 26 recent sessions, newest first, then older ones the cap never reaches.
  const sessions = Array.from({ length: 40 }, (_, index) => ({ sessionId: id(index), cwd: "/", title: `t${index}`, updatedAt: hours(index < 26 ? index : 24 * 20) }));
  sessions[1].cwd = path.join(a.home, "code", "voltagecloud", "backend");
  sessions[2].cwd = path.join(a.home, "code", "voltagecloud", "missing");
  sessions[3].cwd = "/etc";
  sessions[4].cwd = a.paseoHome;
  sessions[5].cwd = path.join(a.home, "code", "voltagecloud", "backend", "..", "..", "..", ".paseo");
  sessions[9].sessionId = "--help";
  sessions[10].cwd = path.join(a.home, "code", "link");
  await setFixture(a.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions }, [tyr.agent]: { mode: "nolist" }, failImports: [id(8)] });
  let result = cli(a.env);
  assert.equal(result.status, 1, `a failed import exits 1: ${result.stderr}`);
  let calls = await imports();
  const fallback = path.join(a.home, "buzz");
  const expected = [...Array.from({ length: 9 }, (_, index) => index), ...Array.from({ length: 11 }, (_, index) => index + 10)];
  assert.deepEqual(calls.map((call) => call.sessionId).sort(), expected.map(id).sort());
  assert.ok(calls.every((call) => call.provider === "brokkr"));
  assert.ok(calls.every((call) => call.password === ""), "paseo import gets the provider's restricted env");
  assert.deepEqual(calls.slice(0, 2).map((call) => call.sessionId).sort(), [id(0), id(1)], "newest first");
  const cwd = Object.fromEntries(calls.map((call) => [call.sessionId, call.cwd]));
  assert.equal(cwd[id(1)], path.join(a.home, "code", "voltagecloud", "backend"), "an existing project directory is kept");
  for (const index of [0, 2, 3, 4, 5]) assert.equal(cwd[id(index)], fallback, `session ${index} falls back to ~/buzz`);
  assert.equal(cwd[id(10)], fallback, "a directory reached through a symlink falls back to ~/buzz");
  const listed = JSON.parse(await readFile(`${fixture}.${brokkr.agent}`, "utf8"));
  assert.equal(listed.lists, 3, "paging stops once the cap is reached");
  assert.deepEqual(listed.argv, ["acp-bridge", "--agent", brokkr.agent, "--channel", brokkr.channel, "--owner", owner]);
  assert.equal(listed.password, "", "the bridge gets the provider env overlay");
  assert.equal(listed.key, a.env.BUZZ_PRIVATE_KEY);
  assert.match(result.stderr, /paseo-restore: done: 19 imported, 0 already present, 1 failed/);
  assert.ok(!result.stderr.includes(a.env.PASEO_PASSWORD) && !result.stderr.includes(a.env.BUZZ_PRIVATE_KEY));

  // A later run (a kept home) leaves a provider with records alone, without
  // starting its bridge. Only the restorer retries the failed import.
  await setFixture(a.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions }, [tyr.agent]: { mode: "nolist" } });
  result = cli(a.env);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(await imports(), []);
  assert.equal(await spawns(), 1, "only tyr's bridge starts");
  assert.match(result.stderr, /left brokkr alone: Paseo already has its agents/);

  // A conversation started in Paseo stores the bridge's own session id, which
  // the listing never shows. Its provider is left alone, so the listed thread
  // of that conversation is not imported as a duplicate.
  const b = await makeHome("b", [brokkr]);
  await writeRecord(b.paseoHome, "live", { id: "live", provider: "brokkr", persistence: { provider: "brokkr", sessionId: randomUUID() } });
  await setFixture(b.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions: listing(3) } });
  result = cli(b.env);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(await imports(), [], "no duplicate import of a live conversation");
  assert.equal(await spawns(), 0);
  // The same when the conversation starts while the bridge lists.
  const c = await makeHome("c", [brokkr]);
  await setFixture(c.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions: listing(3), startsConversation: "brokkr" } });
  result = cli(c.env);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(await spawns(), 1);
  assert.deepEqual(await imports(), [], "no duplicate import of a conversation started during the listing");
  assert.match(result.stderr, /left brokkr alone/);

  // The cap bounds live agents of Buzz providers over all runs: a second run
  // with the budget used imports nothing, and archiving one frees one slot.
  // A native agent does not count, and the cap is not retried.
  const d = await makeHome("d");
  await writeRecord(d.paseoHome, "pa", { id: "pa", provider: "claude", persistence: { provider: "claude", sessionId: "native" } });
  await setFixture(d.home, { ok: { mode: "list", pageSize: 10, sessions: listing(5) } });
  const capped = { ...LIMITS, maxImports: 3 };
  let results = await restore({ providers: [configured("ok", "ok", d.env)], home: d.home, paseoHome: d.paseoHome, limits: capped });
  assert.deepEqual(results.map((entry) => entry.state).sort(), ["imported", "imported", "imported", "over_limit", "over_limit"]);
  assert.equal(failures(results).size, 0, "the cap is final");
  const second = { providers: [configured("ok", "ok2", d.env)], buzzIds: ["ok", "ok2"], home: d.home, paseoHome: d.paseoHome, limits: capped };
  results = await restore(second);
  assert.deepEqual(results.map((entry) => entry.state), Array(5).fill("over_limit"), "the budget is already used");
  assert.equal((await imports()).length, 3);
  const archived = path.join(d.paseoHome, "agents", "imported", `ok-${id(0)}.json`);
  await writeFile(archived, JSON.stringify({ ...JSON.parse(await readFile(archived, "utf8")), archivedAt: hours(1) }));
  results = await restore(second);
  assert.equal(results.filter((entry) => entry.state === "imported").length, 1, "an archived agent frees its slot");

  // A later page that fails keeps the pages before it.
  const e = await makeHome("e");
  await setFixture(e.home, { ok: { mode: "list", pageSize: 2, sessions: listing(5), failFrom: 2 } });
  const logs = [];
  results = await restore({ providers: [configured("ok", "ok", e.env)], home: e.home, paseoHome: e.paseoHome, log: (line) => logs.push(line),
    exec: async () => ({ code: 0 }) });
  assert.deepEqual(results.map((entry) => [entry.sessionId, entry.state]), [[id(0), "imported"], [id(1), "imported"]]);
  assert.ok(logs.includes("kept the first pages of ok's conversations; a later page failed"));

  // A hanging bridge (with a grandchild) and a flooding one are killed at
  // their bounds; the working provider is still restored.
  await setFixture(e.home, {
    hang: { mode: "hang" }, flood: { mode: "flood" }, ok: { mode: "list", pageSize: 10, sessions: listing(5) },
  });
  const limits = { ...LIMITS, requestMs: 1000, listMs: 1500, deadlineMs: 10_000 };
  let started = Date.now();
  const providers = [configured("hang", "hang", e.env), configured("flood", "flood", e.env), configured("ok", "ok", e.env)];
  results = await restore({ providers, home: e.home, paseoHome: e.paseoHome, limits, exec: async () => ({ code: 0 }) });
  assert.ok(Date.now() - started < 5000, "bounded listing");
  assert.deepEqual(results.filter((entry) => entry.state === "list_failed").map((entry) => entry.provider).sort(), ["flood", "hang"]);
  assert.equal(results.filter((entry) => entry.provider === "ok").length, 5);
  assert.deepEqual([...failures(results)].sort(), [["flood", { sessions: null }], ["hang", { sessions: null }]], "failed listings are retried");
  const grandchild = Number(await readFile(`${fixture}.grandchild`, "utf8"));
  for (let wait = 0; wait < 20 && alive(grandchild); wait++) await new Promise((resolve) => setTimeout(resolve, 50));
  assert.ok(!alive(grandchild), "the bridge's process group is killed");

  // At most `concurrency` imports run at once.
  let running = 0;
  let peak = 0;
  const slow = async () => {
    running += 1; peak = Math.max(peak, running);
    await new Promise((resolve) => setTimeout(resolve, 50));
    running -= 1;
    return { code: 0 };
  };
  results = await restore({ providers: [configured("ok", "ok", e.env)], home: e.home, paseoHome: e.paseoHome, limits, exec: slow });
  assert.equal(results.filter((entry) => entry.state === "imported").length, 5);
  assert.equal(peak, 2);

  // An import that never returns is killed at the run deadline; the rest are
  // skipped instead of started, and all five are retried.
  await writeFile(path.join(bin, "stuck"), "#!/bin/sh\nsleep 30 &\nwait\n");
  await chmod(path.join(bin, "stuck"), 0o755);
  started = Date.now();
  results = await restore({ providers: [configured("ok", "ok", e.env)], home: e.home, paseoHome: e.paseoHome, limits: { ...limits, deadlineMs: 2000 },
    exec: (_command, _args, timeoutMs) => run(path.join(bin, "stuck"), [], timeoutMs) });
  assert.ok(Date.now() - started < 4000, "bounded by the run deadline");
  assert.deepEqual(results.map((entry) => entry.state).sort(), ["deadline", "deadline", "deadline", "import_failed", "import_failed"]);
  assert.equal(failures(results).get("ok").sessions.length, 5);

  // The restorer: the daemon is not ready, then one import fails twice. Each
  // failure waits its backoff, a retry imports only the failed session
  // without listing again, and the third failure gives up for good.
  const g = await makeHome("g", [brokkr]);
  await setFixture(g.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions: listing(3) }, failImports: [id(1)], daemonDown: true });
  let clock = 0;
  const restoreLogs = [];
  const fast = { ...LIMITS, readyPolls: 2, readyDelayMs: 1 };
  const restorerFor = (home) => createRestorer({
    pass: (tasks) => restorePass(tasks, { ...home, exec: run, log: (line) => restoreLogs.push(line), limits: fast, baseEnv: home.env }),
    log: (line) => restoreLogs.push(line), clock: () => clock,
  });
  let restorer = restorerFor(g);
  restorer.add(["brokkr"]);
  await restorer.tick();
  assert.deepEqual(await imports(), []);
  assert.ok(restoreLogs.includes("the daemon did not answer with ~/buzz registered"));
  assert.ok(restoreLogs.includes("retrying brokkr in 300 s (attempt 2 of 3)"), restoreLogs.join("\n"));
  clock += 299_000;
  await restorer.tick();
  assert.equal(await spawns(), 0, "no attempt before the backoff");
  await writeFile(fixture, JSON.stringify({ ...JSON.parse(await readFile(fixture, "utf8")), daemonDown: false }));
  clock += 1000;
  await restorer.tick();
  assert.deepEqual((await imports()).map((call) => call.sessionId).sort(), [id(0), id(1), id(2)]);
  assert.ok(restoreLogs.includes("retrying brokkr in 600 s (attempt 3 of 3)"), restoreLogs.join("\n"));
  await rm(`${fixture}.imports`);
  clock += 600_000;
  await restorer.tick();
  assert.deepEqual((await imports()).map((call) => call.sessionId), [id(1)], "only the failed import is retried");
  assert.equal(await spawns(), 1, "a retried import does not list again");
  assert.ok(restoreLogs.includes("gave up restoring brokkr after 3 attempts; import its conversations with Import session"));
  assert.deepEqual(restorer.pending(), []);
  await rm(`${fixture}.imports`);
  clock += 3_600_000;
  await restorer.tick();
  assert.deepEqual(await imports(), [], "nothing runs after giving up");

  // A failed import that the developer imported meanwhile is not imported again.
  const h = await makeHome("h", [brokkr]);
  await setFixture(h.home, { [brokkr.agent]: { mode: "list", pageSize: 10, sessions: listing(2) }, failImports: [id(0)] });
  restoreLogs.length = 0;
  restorer = restorerFor(h);
  restorer.add(["brokkr"]);
  await restorer.tick();
  assert.deepEqual(restorer.pending(), ["brokkr"]);
  await writeRecord(h.paseoHome, "manual", { id: "manual", provider: "brokkr", persistence: { provider: "brokkr", sessionId: id(0) } });
  await rm(`${fixture}.imports`);
  clock += 300_000;
  await restorer.tick();
  assert.deepEqual(await imports(), []);
  assert.deepEqual(restorer.pending(), []);
  assert.ok(restoreLogs.some((line) => line.startsWith("done: 0 imported, 1 already present, 0 failed")), restoreLogs.join("\n"));
  console.log("restore: ok");
} finally {
  await rm(directory, { recursive: true, force: true });
}
