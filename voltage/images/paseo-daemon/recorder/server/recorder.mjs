import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, rename, stat, unlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";

const MAX_JOBS = 256;
const MAX_BYTES = 8 * 1024 * 1024;
const MAX_RECORD_BYTES = 1024 * 1024;
const PART_BYTES = 6000;
const PAGE_SIZE = 100;
const MAX_PAGES = 10;
const TIMELINE_TIMEOUT_MS = 5000;
const MAX_ATTEMPTS = 5;
/// A failing job keeps retrying (at most every minute) for this long before
/// it becomes a dead letter, so relay rollouts and failovers lose nothing.
const RETRY_WINDOW_MS = 24 * 60 * 60 * 1000;
const MAX_DEAD = 32;
const log = (message) => console.error(`buzz-recorder: ${message}`);

function utf8Prefix(value, budget) {
  const bytes = Buffer.from(value, "utf8");
  if (bytes.length <= budget) return value;
  let end = budget;
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end -= 1;
  return bytes.subarray(0, end).toString("utf8");
}

const LABEL = "[A-Za-z_]*(?:password|secret|token|api[_-]?key|private[_-]?key)";
// A run that mixes upper case, lower case and digits, like an encoded secret.
// Hex (commit SHAs, event ids, public keys) and lowercase paths do not match,
// so ordinary answers about code survive.
const opaqueSecret = (run) =>
  !/^[a-f0-9]+$/i.test(run) && /[A-Z]/.test(run) && /[a-z]/.test(run) && /[0-9]/.test(run);

// Drop entire fields containing a credential, rather than risking partial redaction.
export function scrub(value, env = process.env) {
  if (typeof value !== "string") return "";
  const secrets = Object.entries(env).filter(([key, secret]) =>
    /TOKEN|SECRET|PASSWORD|PRIVATE_KEY|API_KEY|KEYPAIR/.test(key) && secret && secret.length >= 6,
  ).map(([, secret]) => secret);
  const runs = value.match(/[A-Za-z0-9+/=_-]{40,}/g) ?? [];
  if (secrets.some((secret) => value.includes(secret)) ||
      runs.some(opaqueSecret) ||
      /-----BEGIN [^-]*PRIVATE KEY-----|\bnsec1[0-9a-z]+|\b(?:gh[pousr]_|github_pat_|xox[baprs]-|sk-[A-Za-z0-9-]|at-[A-Za-z0-9-])\S*|\bAKIA[A-Z0-9]{16}\b|\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/.test(value) ||
      // Only key=value or a quoted key counts as a labeled credential, so
      // prose such as "the token = refreshed hourly" is kept.
      new RegExp(`\\b${LABEL}[=:][^\\s]|["']${LABEL}["']\\s*:\\s*\\S`, "i").test(value)) {
    return "[field omitted: possible credential]";
  }
  return value;
}

function publish(content, channel, { signal, metadata } = {}) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error("shutdown deadline reached")); return; }
    const child = spawn("buzz", ["messages", "send", "--channel", channel, "--content", "-", "--paseo-transcript", JSON.stringify(metadata)], {
      stdio: ["pipe", "pipe", "ignore"], detached: true,
    });
    let output = "";
    let overflow = false;
    const kill = () => {
      try { process.kill(-child.pid, "SIGKILL"); } catch (error) {
        if (error.code !== "ESRCH") reject(new Error("cannot terminate publisher process group"));
      }
    };
    const timeout = setTimeout(kill, 15000);
    signal?.addEventListener("abort", kill, { once: true });
    child.stdout.on("data", (chunk) => {
      if (Buffer.byteLength(output) + chunk.length > 65536) { overflow = true; kill(); }
      else output += chunk;
    });
    child.stdin.on("error", () => {}); // Exit/error below retains the queued job.
    child.on("error", (error) => { clearTimeout(timeout); signal?.removeEventListener("abort", kill); reject(error); });
    child.on("close", (code) => {
      clearTimeout(timeout);
      signal?.removeEventListener("abort", kill);
      let result;
      try { result = JSON.parse(output); } catch { /* Rejected below; job stays. */ }
      if (code === 0 && !overflow && result?.accepted === true && /^[a-f0-9]{64}$/.test(result.event_id)) resolve(result.event_id);
      else reject(new Error("relay did not acknowledge the turn record"));
    });
    child.stdin.end(content);
  });
}

const digest = (value) => createHash("sha256").update(value).digest("hex");

async function timelineMessages(job, paseo, wait, state) {
  let epoch;
  let lastSeq = 0;
  const timeline = paseo.agents.ref(job.agentId).timeline;
  let cursor;
  let entries = [];
  let complete = false;
  let ceiling;
  let retainedBytes = 0;
  for (let page = 0; page < MAX_PAGES; page++) {
    const data = await wait(timeline.refetch({ direction: cursor ? "before" : "tail", cursor, limit: PAGE_SIZE, projection: "projected", timeout: TIMELINE_TIMEOUT_MS }));
    if (data.error || data.staleCursor || data.gap) throw new Error("timeline unavailable or changed; retry retained");
    if (typeof data.epoch !== "string" || !data.epoch || data.epoch.length > 256 || (epoch && epoch !== data.epoch)) throw new Error("timeline epoch unavailable or changed");
    epoch = data.epoch;
    lastSeq = state.epoch === epoch ? state.lastSeq : 0;
    const selected = data.entries.filter((entry) => entry.turnId === job.turnId);
    if (selected.length && ceiling === undefined) ceiling = Math.max(...selected.map((entry) => entry.seqEnd));
    const light = data.entries.map((entry) => {
      const keep = entry.seqEnd > lastSeq && entry.seqEnd <= ceiling && (
        (entry.turnId === job.turnId && ["user_message", "assistant_message"].includes(entry.item.type)) ||
        (job.phase !== "before" && entry.item.type === "user_message"));
      let text;
      if (keep && typeof entry.item.text === "string") {
        retainedBytes += Buffer.byteLength(entry.item.text);
        if (retainedBytes > MAX_RECORD_BYTES) throw new Error("turn exceeds transcript budget; no text truncated");
        text = entry.item.text;
      }
      return { turnId: entry.turnId, seqStart: entry.seqStart, seqEnd: entry.seqEnd,
        item: { type: entry.item.type, ...(text !== undefined ? { text } : {}) } };
    });
    // Retain no tool output or reasoning across pages, only bounded chat text.
    entries = [...light, ...entries];
    if (!data.hasOlder || data.entries.some((entry) => entry.seqEnd <= lastSeq) ||
        (selected.length && data.entries.some((entry) => entry.turnId && entry.turnId !== job.turnId && entry.seqEnd < selected[0].seqStart))) { complete = true; break; }
    if (!data.startCursor) throw new Error("timeline lacks pagination cursor");
    cursor = data.startCursor;
  }
  if (!complete) throw new Error("turn exceeds bounded timeline scan; no text truncated");
  const turn = entries.filter((entry) => entry.turnId === job.turnId);
  const firstSeq = Math.min(...turn.map((entry) => entry.seqStart));
  const lastTurnSeq = Math.max(...turn.map((entry) => entry.seqEnd));
  const messages = [];
  let bytes = 4096;
  for (const entry of entries) {
    if (!["user_message", "assistant_message"].includes(entry.item.type) || typeof entry.item.text !== "string") continue;
    // A prompt queued for the next turn can appear between this turn's
    // assistant messages. Include it in order, but never advance past an
    // assistant message belonging to a still-running later turn.
    if (entry.seqStart < firstSeq || entry.seqEnd > lastTurnSeq) continue;
    if (job.phase === "before" && (entry.item.type !== "user_message" || entry.turnId !== job.turnId)) continue;
    if (entry.item.type === "assistant_message" && entry.turnId !== job.turnId) continue;
    if (!Number.isSafeInteger(entry.seqStart) || !Number.isSafeInteger(entry.seqEnd) || entry.seqStart < 1 || entry.seqEnd < entry.seqStart) throw new Error("timeline lacks stable sequence identity");
    if (entry.seqEnd <= lastSeq || entry.item.text === "") continue;
    const role = entry.item.type === "user_message" ? "developer" : "assistant";
    if (Buffer.byteLength(entry.item.text) > MAX_RECORD_BYTES) throw new Error("message exceeds transcript budget; no text truncated");
    const text = scrub(entry.item.text);
    const id = digest(`${job.agentId}\0${epoch}\0${entry.seqStart}\0${role}`);
    const chunks = [];
    let remaining = text;
    while (remaining) {
      const chunk = utf8Prefix(remaining, PART_BYTES);
      chunks.push(chunk); remaining = remaining.slice(chunk.length);
    }
    for (const [index, content] of chunks.entries()) {
      const message = { content, role, entry: id, part: index + 1, parts: chunks.length, seqEnd: entry.seqEnd };
      bytes += Buffer.byteLength(JSON.stringify(message)) + 700; // Frozen metadata and atomic retry fields.
      if (bytes > MAX_RECORD_BYTES) throw new Error("turn exceeds transcript budget; no text truncated");
      messages.push(message);
    }
  }
  return { messages, epoch };
}

/** Persist bounded full-transcript intents before fetching or publishing. */
export function createRecorder({ directory, channel, send = publish, retryMs = 1000, shutdownMs = 1500, retryWindowMs = RETRY_WINDOW_MS }) {
  let paseo;
  let timer;
  let closed = false;
  let queue = Promise.resolve();
  let pendingCaptures = 0;
  let flushing;
  let closing;
  let order = Date.now() * 1000;
  const stop = new AbortController();
  const wait = (operation) => new Promise((resolve, reject) => {
    const aborted = () => reject(new Error("shutdown flush deadline reached"));
    // The caller may already have started an operation that can reject.
    // Consume it even when shutdown expired during the preceding disk read.
    Promise.resolve(operation).then(resolve, reject).finally(() => stop.signal.removeEventListener("abort", aborted));
    if (stop.signal.aborted) return aborted();
    stop.signal.addEventListener("abort", aborted, { once: true });
  });
  const serialize = (fn) => { const result = queue.then(fn); queue = result.catch(() => {}); return result; };
  const files = async () => {
    await mkdir(directory, { recursive: true, mode: 0o700 });
    const names = await readdir(directory);
    for (const name of names.filter((file) => /^[a-f0-9]{64}(?:\.state)?\.json\.tmp$/.test(file))) await unlink(path.join(directory, name));
    return names.filter((file) => /^[a-f0-9]{64}(?:\.state|\.dead)?\.json$/.test(file));
  };
  const read = async (file, fallback) => {
    try { return JSON.parse(await readFile(file, "utf8")); }
    catch (error) { if (error.code === "ENOENT") return fallback; throw error; }
  };
  const write = async (file, value) => {
    const text = JSON.stringify(value);
    const bytes = Buffer.byteLength(text);
    if (bytes > MAX_RECORD_BYTES) throw new Error("transcript job exceeds one MiB; no text truncated");
    const names = await files();
    const sizes = await Promise.all(names.map((name) => stat(path.join(directory, name))));
    // Include the temporary copy: atomic replacement never exceeds disk cap.
    const total = sizes.reduce((sum, info) => sum + info.size, 0);
    const old = sizes[names.indexOf(path.basename(file))]?.size ?? 0;
    if (total - old + bytes > MAX_BYTES - MAX_RECORD_BYTES || total + bytes > MAX_BYTES) throw new Error("recording spool byte budget exhausted");
    await writeFile(`${file}.tmp`, text, { mode: 0o600 });
    await rename(`${file}.tmp`, file);
  };
  const schedule = (delay) => {
    if (closed || timer || flushing) return;
    timer = setTimeout(() => { timer = undefined; flush().catch(() => log("flush failed; intents retained")); }, delay);
    timer.unref?.();
  };
  const flush = (shutdownSeen) => {
    if (flushing) return flushing;
    let pending = false;
    let retryDelay = retryMs;
    flushing = serialize(async () => {
      const jobs = [];
      for (const name of await files()) {
        if (name.includes(".state.") || name.includes(".dead.")) continue;
        const file = path.join(directory, name);
        let job;
        try { job = await read(file); }
        catch (error) {
          if (!(error instanceof SyntaxError)) throw error;
          await rename(file, file.replace(/\.json$/, ".dead.json"));
          log("malformed recording retained as dead letter"); continue;
        }
        if (job?.version !== 2) { await rename(file, file.replace(/\.json$/, ".dead.json")); log("unsupported old recording format retained as dead letter"); continue; }
        if (Number.isSafeInteger(job.order)) order = Math.max(order, job.order);
        jobs.push({ name, job });
      }
      // Preserve per-session order while rotating failures across sessions.
      jobs.sort((a, b) => a.job.order - b.job.order || a.name.localeCompare(b.name));
      const heads = new Map();
      for (const candidate of jobs) if (!heads.has(candidate.job.session)) heads.set(candidate.job.session, candidate);
      const ready = [...heads.values()].filter(({ name }) => !shutdownSeen?.has(name));
      ready.sort((a, b) => (a.job.attempts ?? 0) - (b.job.attempts ?? 0) || a.job.order - b.job.order);
      pending = jobs.length > 8;
      for (const { name, job } of ready.slice(0, 8)) {
        if (stop.signal.aborted) break;
        shutdownSeen?.add(name);
        const file = path.join(directory, name);
        try {
          const stateFile = path.join(directory, `${job.session}.state.json`);
          const state = await read(stateFile, { root: null, lastSeq: 0, sequence: 0, createdAt: 0 });
          if (!job.messages) {
            if (!paseo) { pending = true; continue; }
            const captured = await timelineMessages(job, paseo, wait, state);
            job.messages = captured.messages; job.epoch = captured.epoch;
            job.next = 0;
            await write(file, job);
          }
          for (; job.next < job.messages.length; job.next++) {
            if (stop.signal.aborted) break;
            const message = job.messages[job.next];
            if (!message.metadata) {
              // Ordinary clients sort by integer created_at. Give each part a
              // distinct wall-clock second, never a future timestamp. This
              // keeps flat replies readable without growing thread depth.
              const delay = (state.createdAt + 1) * 1000 - Date.now();
              if (delay > 5000) throw new Error("clock behind transcript cursor; retry retained");
              if (delay > 0) await wait(new Promise((resolve) => setTimeout(resolve, delay)));
              message.metadata = { session: job.session, entry: message.entry, role: message.role,
                part: message.part, parts: message.parts, sequence: state.sequence + 1,
                created_at: Math.floor(Date.now() / 1000), ...(state.root ? { root: state.root } : {}) };
              await write(file, job); // Freeze event identity before first send.
            }
            if (stop.signal.aborted) break;
            const eventId = await wait(send(message.content, channel, { signal: stop.signal, metadata: message.metadata }));
            if (stop.signal.aborted) break;
            if (!/^[a-f0-9]{64}$/.test(eventId ?? "")) throw new Error("publisher omitted accepted event identity");
            state.root ??= eventId;
            if (job.epoch && state.epoch !== job.epoch) { state.epoch = job.epoch; state.lastSeq = 0; }
            state.createdAt = message.metadata.created_at;
            state.sequence = message.metadata.sequence;
            if (message.part === message.parts) state.lastSeq = Math.max(state.lastSeq, message.seqEnd);
            await write(stateFile, state);
            // Persist the ACK before advancing. A crash in this prefix retries
            // the exact event, including the original root/no-root metadata.
            job.next += 1; await write(file, job); job.next -= 1;
          }
          if (!stop.signal.aborted && job.next >= job.messages.length) await unlink(file);
          else pending = true;
        } catch (error) {
          if (stop.signal.aborted) break;
          job.attempts = Math.min((Number.isSafeInteger(job.attempts) ? job.attempts : 0) + 1, MAX_ATTEMPTS);
          if (!Number.isSafeInteger(job.failingSince)) job.failingSince = Date.now();
          retryDelay = Math.max(retryDelay, retryMs * 2 ** job.attempts);
          await write(file, job);
          if (job.attempts >= MAX_ATTEMPTS && Date.now() - job.failingSince >= retryWindowMs) {
            await rename(file, file.replace(/\.json$/, ".dead.json"));
            await pruneDead();
            log("delivery kept failing for the whole retry window; dead letter retained");
          } else { pending = true; log("delivery failed; rotated durable retry retained"); }
        }
      }
      if (jobs.length > ready.slice(0, 8).length) pending = true;
    }).finally(() => { flushing = undefined; if (pending) schedule(Math.min(60000, retryDelay)); });
    return flushing;
  };
  // Keep only the newest dead letters, so they never fill the spool.
  const pruneDead = async () => {
    const dead = (await files()).filter((name) => name.endsWith(".dead.json"));
    if (dead.length <= MAX_DEAD) return;
    const aged = await Promise.all(dead.map(async (name) => ({ name, mtime: (await stat(path.join(directory, name))).mtimeMs })));
    aged.sort((left, right) => left.mtime - right.mtime);
    for (const { name } of aged.slice(0, dead.length - MAX_DEAD)) await unlink(path.join(directory, name));
  };
  // Free the least recently used cursor of a session with no pending job. A
  // later turn of that session starts a new recording thread.
  const evictIdleSession = async (names) => {
    const busy = new Set();
    for (const name of names.filter((name) => name.endsWith(".json") && !name.includes(".state."))) {
      const job = await read(path.join(directory, name), {});
      if (typeof job.session === "string") busy.add(job.session);
    }
    const idle = await Promise.all(names.filter((name) => name.endsWith(".state.json") && !busy.has(name.slice(0, -".state.json".length)))
      .map(async (name) => ({ name, mtime: (await stat(path.join(directory, name))).mtimeMs })));
    if (!idle.length) return false;
    idle.sort((left, right) => left.mtime - right.mtime);
    await unlink(path.join(directory, idle[0].name));
    log("session cursor limit reached; evicted the least recently used idle session");
    return true;
  };
  return {
    observe(context) { paseo = context.paseo; schedule(0); },
    capture(payload, context, phase = "after") {
      if (payload.agent.provider !== "claude") return Promise.resolve();
      const { id: agentId } = payload.agent; const { turnId } = payload;
      if (typeof agentId !== "string" || !agentId || agentId.length > 256 || typeof turnId !== "string" || !turnId || turnId.length > 256) return Promise.reject(new Error("cannot record a turn without a bounded identity"));
      if (closed) return Promise.reject(new Error("recorder is closed"));
      if (pendingCaptures >= MAX_JOBS) return Promise.reject(new Error("recording queue full; turn refused"));
      paseo = context.paseo;
      const id = digest(`${agentId}\0${turnId}\0${phase}`); const session = digest(agentId);
      pendingCaptures += 1;
      return serialize(async () => {
        const names = await files();
        if (!names.includes(`${id}.json`) && !names.includes(`${id}.dead.json`)) {
          if (names.filter((name) => !name.includes(".state.") && !name.includes(".dead.")).length >= MAX_JOBS) throw new Error("recording spool full; turn refused");
          if (!names.includes(`${session}.state.json`) && names.filter((name) => name.includes(".state.")).length >= MAX_JOBS &&
              !(await evictIdleSession(names))) throw new Error("recording session capacity exhausted");
          await write(path.join(directory, `${id}.json`), { version: 2, id, session, agentId, turnId, phase, order: ++order });
        }
        schedule(0);
      }).finally(() => { pendingCaptures -= 1; });
    },
    flush,
    close() {
      if (closing) return closing;
      closed = true; clearTimeout(timer);
      const deadline = setTimeout(() => stop.abort(), shutdownMs);
      closing = (async () => {
        await queue;
        const seen = new Set();
        for (let batch = 0; batch < MAX_JOBS && !stop.signal.aborted; batch++) {
          const active = (await files()).filter((name) => !name.includes(".state.") && !name.includes(".dead.") && !seen.has(name));
          if (!active.length) break;
          const before = seen.size;
          await flush(seen);
          if (seen.size === before) break;
        }
        if (stop.signal.aborted) log("shutdown deadline reached; pod-local unsent messages may be lost on replacement");
      })().finally(() => clearTimeout(deadline));
      return closing;
    },
  };
}

export default function contribute(server) {
  const channel = process.env.BUZZ_PASEO_RECORD_CHANNEL;
  if (!channel) { log("BUZZ_PASEO_RECORD_CHANNEL is unset; recording disabled"); return () => {}; }
  const recorder = createRecorder({ directory: path.join(process.env.PASEO_HOME, "buzz-recording"), channel });
  const stopOpen = server.addHook("agent.open.before", (payload, context) => { recorder.observe(context); return payload.request; });
  const stopBefore = server.addHook("agent.turn.before", (payload, context) => payload.turnId ? recorder.capture(payload, context, "before") : undefined);
  const stopTurn = server.addHook("agent.turn.after", (payload, context) => recorder.capture(payload, context));
  recorder.flush().catch(() => log("startup replay failed; retry files retained"));
  const shutdown = () => recorder.close().catch(() => log("shutdown flush failed; retry files retained"));
  process.once("SIGTERM", shutdown); process.once("SIGINT", shutdown);
  return async () => { stopOpen(); stopBefore(); stopTurn(); try { await recorder.close(); }
    finally { process.removeListener("SIGTERM", shutdown); process.removeListener("SIGINT", shutdown); } };
}
