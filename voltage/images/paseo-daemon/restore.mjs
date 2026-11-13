#!/usr/bin/env node
// Import each Buzz remote agent's recent conversations into a fresh daemon.
// The daemon's home is pod-local, so after a replacement Paseo knows no
// agents, although every conversation is still a Buzz thread on the relay.
// For each Buzz provider in config.json that has no Paseo agent record at
// all, this runs the provider's own bridge command, lists its sessions over
// ACP (`session/list`), and runs `paseo import` for the newest ones. A
// provider with any record is left alone: its home was kept, or it has a
// conversation started in Paseo, whose record holds the bridge's own session
// id instead of the listed thread id. So a restore never imports a duplicate.
// `paseo-agents-watch` drives the restorer, which retries a failed provider
// with backoff and then gives up. Best-effort and bounded: every process has
// a deadline and is killed with its process group, output is capped, and a
// failure only logs to stderr. Never prints secret values.
import { spawn } from "node:child_process";
import { readFile, readdir, realpath, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { isBuzzProvider } from "./providers.mjs";

export const LIMITS = Object.freeze({
  /** The newest conversations restored per provider. */
  perProvider: 20,
  /** Conversations with no activity for longer stay importable from the UI. */
  maxAgeMs: 14 * 24 * 60 * 60_000,
  /**
   * Imports stop once the Buzz providers have this many non-archived Paseo
   * agents, restored or started in Paseo, over all runs. Each such agent can
   * keep one idle bridge process (about 7 MiB RSS) in the daemon.
   */
  maxImports: 40,
  /** `session/list` pages read per provider. */
  maxPages: 10,
  /** One ACP request to a bridge, and one `paseo project ls`. */
  requestMs: 30_000,
  /** One provider's bridge, from spawn to kill. */
  listMs: 60_000,
  /** One `paseo import`. */
  importMs: 60_000,
  /** The whole run. */
  deadlineMs: 5 * 60_000,
  /** Bridges or imports running at once. */
  concurrency: 2,
  /** Bridge stdout bytes, and the longest JSON-RPC line. */
  maxOutputBytes: 4 * 1024 * 1024,
  maxLineBytes: 1024 * 1024,
  /** Polls of `paseo project ls` until the daemon answers with ~/buzz registered. */
  readyPolls: 60,
  readyDelayMs: 2000,
  /** Waits before the second and the third attempt of a failed provider. Then it gives up. */
  retryDelaysMs: Object.freeze([5 * 60_000, 10 * 60_000]),
});

/** Thread root ids today; anything that cannot be read as an option. */
const SESSION_ID = /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$/;
const MAX_RECORD_FILES = 10_000;
const MAX_RECORD_BYTES = 1024 * 1024;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const reason = (error) => String(error?.message ?? error).slice(0, 200);

/** Kill a detached child's whole process group; it may already be gone. */
function killGroup(child) {
  try { process.kill(-child.pid, "SIGKILL"); } catch { /* Exited or never started. */ }
}

/**
 * Run a command in its own process group with `env`, capturing at most 1 MiB
 * of stdout. The group is killed at `timeoutMs`. Resolves `{code, stdout}`;
 * code -1 when the command could not start or timed out.
 */
export function run(command, args, timeoutMs, env = process.env) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { env, detached: true, stdio: ["ignore", "pipe", "ignore"] });
    let stdout = "";
    let timedOut = false;
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      if (stdout.length < LIMITS.maxLineBytes) stdout += chunk;
    });
    const timer = setTimeout(() => { timedOut = true; killGroup(child); }, Math.max(1, timeoutMs));
    child.on("error", () => { clearTimeout(timer); resolve({ code: -1, stdout }); });
    child.on("close", (code) => {
      clearTimeout(timer);
      killGroup(child);
      resolve({ code: timedOut ? -1 : code, stdout });
    });
  });
}

/** A minimal ACP client over a bridge's stdio. `close` kills its process group. */
function connect(command, env, limits) {
  const child = spawn(command[0], command.slice(1), { env, detached: true, stdio: ["pipe", "pipe", "ignore"] });
  const pending = new Map();
  let failure = null;
  let buffer = "";
  let received = 0;
  let nextId = 1;
  const fail = (error) => {
    failure ??= error;
    for (const entry of pending.values()) entry.reject(failure);
    pending.clear();
  };
  child.on("error", () => fail(new Error("bridge did not start")));
  child.on("close", () => fail(new Error("bridge exited")));
  child.stdin.on("error", () => fail(new Error("bridge closed its input")));
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    received += Buffer.byteLength(chunk);
    if (received > limits.maxOutputBytes) return fail(new Error("bridge output exceeded its bound"));
    buffer += chunk;
    for (let at = buffer.indexOf("\n"); at >= 0; at = buffer.indexOf("\n")) {
      const line = buffer.slice(0, at);
      buffer = buffer.slice(at + 1);
      let message;
      try { message = JSON.parse(line); } catch { continue; }
      if (message?.method !== undefined) {
        // A request from the bridge: this client offers no capabilities.
        if (message.id !== undefined) send({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: "not supported" } });
        continue;
      }
      const entry = pending.get(message?.id);
      if (!entry) continue;
      pending.delete(message.id);
      if (message.error) entry.reject(new Error("bridge returned an error"));
      else entry.resolve(message.result);
    }
    if (Buffer.byteLength(buffer) > limits.maxLineBytes) fail(new Error("bridge line exceeded its bound"));
  });
  function send(message) {
    if (!child.stdin.destroyed) child.stdin.write(`${JSON.stringify(message)}\n`);
  }
  const request = (method, params, timeoutMs) => new Promise((resolve, reject) => {
    if (failure) return reject(failure);
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out`));
    }, Math.max(1, timeoutMs));
    pending.set(id, {
      resolve: (value) => { clearTimeout(timer); resolve(value); },
      reject: (error) => { clearTimeout(timer); reject(error); },
    });
    send({ jsonrpc: "2.0", id, method, params });
  });
  const close = () => {
    child.stdin.destroy();
    killGroup(child);
  };
  return { request, close };
}

/**
 * The provider's sessions updated since `now - maxAgeMs`, newest first, at
 * most `perProvider`, as `{sessions, complete}`. The bridge lists newest
 * participation first, so paging stops at the first page with only older
 * sessions. A failed first request throws; a failed later page keeps the
 * earlier pages and sets `complete` to false.
 */
export async function listRecent({ command, env, now, limits, deadline }) {
  const until = Math.min(deadline, Date.now() + limits.listMs);
  const remaining = () => Math.min(limits.requestMs, until - Date.now());
  const cutoff = now - limits.maxAgeMs;
  const bridge = connect(command, env, limits);
  const timer = setTimeout(bridge.close, Math.max(1, until - Date.now()));
  try {
    const initialized = await bridge.request("initialize", {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
      clientInfo: { name: "paseo-restore", version: "1" },
    }, remaining());
    if (!initialized?.agentCapabilities?.sessionCapabilities?.list) return { sessions: [], complete: true };
    const found = new Map();
    let cursor = null;
    let complete = true;
    for (let page = 0; page < limits.maxPages && found.size < limits.perProvider; page++) {
      let result;
      try {
        result = await bridge.request("session/list", cursor ? { cursor } : {}, remaining());
      } catch (error) {
        if (page === 0) throw error;
        complete = false;
        break;
      }
      const sessions = Array.isArray(result?.sessions) ? result.sessions : [];
      let recent = 0;
      for (const session of sessions) {
        const updated = Date.parse(session?.updatedAt);
        if (!(updated >= cutoff)) continue;
        recent += 1;
        if (typeof session.sessionId !== "string" || !SESSION_ID.test(session.sessionId) || found.has(session.sessionId)) continue;
        found.set(session.sessionId, { sessionId: session.sessionId, cwd: typeof session.cwd === "string" ? session.cwd : "", updated });
      }
      const next = typeof result?.nextCursor === "string" && result.nextCursor ? result.nextCursor : null;
      if (!next || next === cursor || (sessions.length > 0 && recent === 0)) break;
      cursor = next;
    }
    const sessions = [...found.values()].sort((a, b) => b.updated - a.updated).slice(0, limits.perProvider);
    return { sessions, complete };
  } finally {
    clearTimeout(timer);
    bridge.close();
  }
}

/**
 * What the daemon's agent records in `agentsDir` say, archived ones
 * included: `sessions` holds `provider\0sessionId` keys, `providers` every
 * provider with a record, and `live` each provider's non-archived record
 * count. `paseo agent ls --json` does not show session ids, so this reads the
 * records directly: the same records the daemon checks before an import.
 */
export async function agentRecords(agentsDir) {
  const sessions = new Set();
  const providers = new Set();
  const live = new Map();
  const files = [];
  let entries = [];
  try { entries = await readdir(agentsDir, { withFileTypes: true }); } catch { return { sessions, providers, live }; }
  for (const entry of entries) {
    if (files.length >= MAX_RECORD_FILES) break;
    const full = path.join(agentsDir, entry.name);
    if (entry.isFile() && entry.name.endsWith(".json")) files.push(full);
    else if (entry.isDirectory()) {
      let nested = [];
      try { nested = await readdir(full, { withFileTypes: true }); } catch { continue; }
      for (const file of nested) {
        if (file.isFile() && file.name.endsWith(".json") && files.length < MAX_RECORD_FILES) files.push(path.join(full, file.name));
      }
    }
  }
  for (const file of files) {
    try {
      if ((await stat(file)).size > MAX_RECORD_BYTES) continue;
      const record = JSON.parse(await readFile(file, "utf8"));
      const provider = record?.persistence?.provider ?? record?.provider;
      if (typeof provider !== "string") continue;
      providers.add(provider);
      if (!record.archivedAt) live.set(provider, (live.get(provider) ?? 0) + 1);
      for (const handle of [record.persistence?.sessionId, record.persistence?.nativeHandle]) {
        if (typeof handle === "string") sessions.add(`${provider}\0${handle}`);
      }
    } catch { /* A record being written or unreadable: the daemon also refuses duplicates. */ }
  }
  return { sessions, providers, live };
}

/**
 * The listed cwd when it is an existing directory strictly under `home`,
 * outside hidden directories and reached without symlinks; otherwise
 * `fallback`. The bridge lists "/" for conversations without a project.
 */
export async function importCwd(listed, home, fallback) {
  if (typeof listed !== "string" || !path.isAbsolute(listed)) return fallback;
  const resolved = path.resolve(listed);
  const relative = path.relative(home, resolved);
  if (!relative || path.isAbsolute(relative) || relative.split(path.sep).some((part) => part.startsWith("."))) return fallback;
  try {
    if ((await realpath(resolved)) !== resolved || !(await stat(resolved)).isDirectory()) return fallback;
  } catch { return fallback; }
  return resolved;
}

/** Run `worker` over `items`, at most `limit` at once. */
async function pool(items, limit, worker) {
  let next = 0;
  const lanes = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) await worker(items[next++]);
  });
  await Promise.all(lanes);
}

/**
 * Restore `providers` (`[{id, command, env, sessions?}]`). A provider without
 * `sessions` is listed, and only when Paseo has no record of it before and
 * after the listing. A provider with `sessions` (a retry) imports exactly
 * those. `buzzIds` are the Buzz provider ids whose non-archived agents count
 * against `maxImports`. Returns one `{provider, state}` per provider that was
 * not listed and one `{provider, sessionId, session, state, cwd?}` per
 * session. Provider states: "not_fresh", "list_failed", "deadline". Session
 * states: "imported", "known", "import_failed", "over_limit", "deadline".
 */
export async function restore({ providers, buzzIds = providers.map((provider) => provider.id), home, paseoHome, exec = run, log = () => {},
  limits = LIMITS, now = Date.now() }) {
  const deadline = Date.now() + limits.deadlineMs;
  const root = await realpath(home);
  const fallback = path.join(root, "buzz");
  const agentsDir = path.join(paseoHome, "agents");
  const results = [];
  const pending = [];
  const listed = new Set();
  const notFresh = (id) => {
    log(`left ${id} alone: Paseo already has its agents`);
    results.push({ provider: id, state: "not_fresh" });
  };
  const before = await agentRecords(agentsDir);
  await pool(providers, limits.concurrency, async (provider) => {
    if (provider.sessions) {
      for (const session of provider.sessions) pending.push({ provider, ...session });
      return;
    }
    if (before.providers.has(provider.id)) return notFresh(provider.id);
    if (Date.now() >= deadline) return results.push({ provider: provider.id, state: "deadline" });
    try {
      const { sessions, complete } = await listRecent({ command: provider.command, env: provider.env, now, limits, deadline });
      if (!complete) log(`kept the first pages of ${provider.id}'s conversations; a later page failed`);
      listed.add(provider.id);
      for (const session of sessions) pending.push({ provider, ...session });
    } catch (error) {
      log(`could not list ${provider.id}'s conversations: ${reason(error)}`);
      results.push({ provider: provider.id, state: "list_failed" });
    }
  });
  // Read again: a conversation started in Paseo during the listing makes the
  // provider's home no longer fresh, and its agents count against the budget.
  const records = await agentRecords(agentsDir);
  for (const id of listed) if (records.providers.has(id)) notFresh(id);
  const queue = pending
    .filter(({ provider }) => !listed.has(provider.id) || !records.providers.has(provider.id))
    .sort((a, b) => b.updated - a.updated);
  let budget = Math.max(0, limits.maxImports - buzzIds.reduce((sum, id) => sum + (records.live.get(id) ?? 0), 0));
  await pool(queue, limits.concurrency, async ({ provider, sessionId, cwd: listedCwd, updated }) => {
    const result = { provider: provider.id, sessionId, session: { sessionId, cwd: listedCwd, updated } };
    results.push(result);
    if (records.sessions.has(`${provider.id}\0${sessionId}`)) result.state = "known";
    else if (budget <= 0) result.state = "over_limit";
    else if (Date.now() >= deadline) result.state = "deadline";
    else {
      budget -= 1;
      result.cwd = await importCwd(listedCwd, root, fallback);
      const imported = await exec("paseo", ["import", "--provider", provider.id, "--cwd", result.cwd, "--", sessionId],
        Math.min(limits.importMs, deadline - Date.now()), provider.env);
      result.state = imported.code === 0 ? "imported" : "import_failed";
      if (imported.code !== 0) log(`could not import a conversation of ${provider.id}`);
    }
  });
  const count = (state) => results.filter((result) => result.state === state).length;
  log(`done: ${count("imported")} imported, ${count("known")} already present, ${count("import_failed") + count("list_failed")} failed, `
    + `${count("deadline") + count("over_limit")} skipped by bounds`);
  return results;
}

/**
 * The providers of `results` to try again, as a Map from id to
 * `{sessions}`: null to list again after a listing failure, otherwise the
 * sessions whose import failed or hit the deadline. The import cap is final.
 */
export function failures(results) {
  const retry = new Map();
  for (const result of results) {
    if (result.sessionId === undefined) {
      if (result.state === "list_failed" || result.state === "deadline") retry.set(result.provider, { sessions: null });
    } else if (result.state === "import_failed" || result.state === "deadline") {
      const entry = retry.get(result.provider) ?? { sessions: [] };
      entry.sessions.push(result.session);
      retry.set(result.provider, entry);
    }
  }
  return retry;
}

/**
 * Whether the daemon answers `paseo project ls` with `project` registered,
 * polling at most `readyPolls` times. `paseo-workspaces` registers ~/buzz
 * before it clones any repository.
 */
export async function daemonReady({ project, exec = run, limits = LIMITS }) {
  for (let poll = 0; poll < limits.readyPolls; poll++) {
    if (poll > 0) await sleep(limits.readyDelayMs);
    const listed = await exec("paseo", ["project", "ls", "--json"], limits.requestMs);
    if (listed.code !== 0) continue;
    try {
      const projects = JSON.parse(listed.stdout);
      if (Array.isArray(projects) && projects.some((entry) => path.resolve(String(entry?.path ?? "")) === project)) return true;
    } catch { /* Not ready. */ }
  }
  return false;
}

/**
 * The Buzz providers in the daemon's config.json, with the environment the
 * daemon launches them with.
 */
export async function buzzProvidersFromConfig(configFile, baseEnv = process.env) {
  const config = JSON.parse(await readFile(configFile, "utf8"));
  return Object.entries(config?.agents?.providers ?? {})
    .filter(([, entry]) => isBuzzProvider(entry) && entry.enabled !== false)
    .filter(([, entry]) => Array.isArray(entry.command) && entry.command.every((part) => typeof part === "string"))
    .map(([id, entry]) => ({ id, command: entry.command, env: { ...baseEnv, ...entry.env } }));
}

/**
 * One restore attempt for `tasks` (`[{id, sessions}]`, `sessions` as in
 * `failures`). Waits for the daemon first. A task whose provider is no longer
 * configured is dropped. Returns `failures` of the attempt; every task fails
 * when the daemon is not ready.
 */
export async function restorePass(tasks, { configFile, home, paseoHome, exec = run, log = () => {}, limits = LIMITS, baseEnv = process.env }) {
  const configured = await buzzProvidersFromConfig(configFile, baseEnv);
  const byId = new Map(configured.map((provider) => [provider.id, provider]));
  const providers = tasks.filter((task) => byId.has(task.id)).map((task) => ({ ...byId.get(task.id), sessions: task.sessions }));
  if (providers.length === 0) return new Map();
  if (!(await daemonReady({ project: path.join(home, "buzz"), exec, limits }))) {
    log("the daemon did not answer with ~/buzz registered");
    return new Map(providers.map((provider) => [provider.id, { sessions: provider.sessions }]));
  }
  const results = await restore({ providers, buzzIds: configured.map((provider) => provider.id), home, paseoHome, exec, log, limits });
  return failures(results);
}

/**
 * Restore attempts per provider, one `pass` at a time. `add(ids)` queues
 * providers; `tick()` runs the due ones and resolves when that pass ends.
 * A provider that `pass` reports as failed is tried again after each of
 * `delaysMs`, and then given up with a log line. `pass(tasks)` resolves to a
 * Map like `failures`; a throwing pass fails every task.
 */
export function createRestorer({ pass, log = () => {}, delaysMs = LIMITS.retryDelaysMs, clock = Date.now }) {
  const tasks = new Map();
  const attempts = delaysMs.length + 1;
  let running = null;
  async function attempt(due) {
    let failed;
    try {
      failed = await pass(due.map(([id, task]) => ({ id, sessions: task.sessions })));
    } catch (error) {
      log(`restore failed: ${reason(error)}`);
      failed = new Map(due.map(([id, task]) => [id, { sessions: task.sessions }]));
    }
    for (const [id, task] of due) {
      task.attempt += 1;
      const failure = failed.get(id);
      if (!failure) tasks.delete(id);
      else if (task.attempt >= attempts) {
        tasks.delete(id);
        log(`gave up restoring ${id} after ${task.attempt} attempts; import its conversations with Import session`);
      } else {
        const delay = delaysMs[task.attempt - 1];
        task.sessions = failure.sessions;
        task.dueAt = clock() + delay;
        log(`retrying ${id} in ${Math.round(delay / 1000)} s (attempt ${task.attempt + 1} of ${attempts})`);
      }
    }
  }
  return {
    add(ids) {
      for (const id of ids) if (!tasks.has(id)) tasks.set(id, { attempt: 0, dueAt: clock(), sessions: null });
    },
    pending: () => [...tasks.keys()],
    tick() {
      if (running) return running;
      const due = [...tasks].filter(([, task]) => task.dueAt <= clock());
      if (due.length === 0) return Promise.resolve();
      running = attempt(due).finally(() => { running = null; });
      return running;
    },
  };
}

/** The production restore attempt for `createRestorer`. */
export function daemonPass(log) {
  return (tasks) => restorePass(tasks, {
    configFile: path.join(process.env.PASEO_HOME, "config.json"),
    home: process.env.HOME,
    paseoHome: process.env.PASEO_HOME,
    log,
  });
}

// One attempt for the configured Buzz providers (all, or only the ids given),
// without retries. Exits 1 when something failed.
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const log = (message) => process.stderr.write(`paseo-restore: ${message}\n`);
  const ids = process.argv.slice(2);
  try {
    const configured = await buzzProvidersFromConfig(path.join(process.env.PASEO_HOME, "config.json"));
    const tasks = configured.filter((provider) => ids.length === 0 || ids.includes(provider.id)).map(({ id }) => ({ id, sessions: null }));
    if ((await daemonPass(log)(tasks)).size > 0) process.exitCode = 1;
  } catch (error) {
    log(`restore failed: ${reason(error)}`);
    process.exitCode = 1;
  }
}
