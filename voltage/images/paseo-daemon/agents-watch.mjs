#!/usr/bin/env node
// Keep the daemon's Buzz remote-agent providers in step with the manager's
// agent list without restarting the daemon. Kubernetes refreshes the mounted
// copy of the daemon Secret's BUZZ_PASEO_AGENTS key after the manager changes
// it; this rewrites config.json's Buzz providers and runs `paseo reload`.
// It also drives the conversation restore (restore.mjs): every configured
// provider at startup and each provider that a reload adds, with retries.
// An absent file (no mount) does nothing. An invalid list is logged and the
// current providers stay. Never prints secret values.
import { spawn } from "node:child_process";
import { readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { withAgents } from "./providers.mjs";
import { buzzProvidersFromConfig, createRestorer, daemonPass } from "./restore.mjs";

const MAX_FILE_BYTES = 256 * 1024;
const RELOAD_TIMEOUT_MS = 60_000;

/** The canonical text of an agent list, so formatting differences do not reload. */
const canonical = (agents) => JSON.stringify(agents);

/**
 * Ids of the agents in `agents` that `applied` does not have. A changed
 * agent is not added: Paseo already has its conversations as agents.
 */
function addedIds(agents, applied) {
  let before = [];
  try { before = JSON.parse(applied ?? "[]"); } catch { /* Nothing was applied. */ }
  const previous = new Set((Array.isArray(before) ? before : []).map((agent) => agent?.id));
  return agents.filter((agent) => !previous.has(agent.id)).map((agent) => agent.id);
}

/**
 * One check. `state.applied` is the canonical list last applied. Returns the
 * new state and what happened: "absent", "unchanged", "invalid", "reload_failed"
 * or "reloaded". A reload also returns `added`, the ids of new agents.
 */
export async function checkOnce({ state, readAgents, readConfig, writeConfig, reload, owner, log }) {
  const text = await readAgents();
  if (text === null) return { state, outcome: "absent" };
  let agents;
  try {
    if (Buffer.byteLength(text) > MAX_FILE_BYTES) throw new Error("the agent list is too large");
    agents = JSON.parse(text);
  } catch (error) {
    log(`ignored the agent list: ${error.message.slice(0, 200)}`);
    return { state, outcome: "invalid" };
  }
  if (canonical(agents) === state.applied) return { state, outcome: "unchanged" };
  let next;
  try {
    next = withAgents(await readConfig(), agents, owner);
  } catch (error) {
    log(`kept the current agents: ${error.message.slice(0, 200)}`);
    return { state, outcome: "invalid" };
  }
  await writeConfig(next);
  if (!(await reload())) {
    log("paseo reload failed; retrying on the next check");
    return { state, outcome: "reload_failed" };
  }
  log(`reloaded ${agents.length} Buzz agent provider(s)`);
  return { state: { applied: canonical(agents) }, outcome: "reloaded", added: addedIds(agents, state.applied) };
}

/**
 * One loop iteration: `checkOnce`, queue the added providers on `restorer`
 * (from `createRestorer`), and start its due restore attempts. Returns the
 * check, without waiting for the attempts.
 */
export async function watchOnce({ restorer, ...check }) {
  try {
    const result = await checkOnce(check);
    if (result.added?.length) restorer.add(result.added);
    return result;
  } finally {
    // Not awaited: an attempt can take minutes, and checks continue meanwhile.
    restorer.tick();
  }
}

function reloadDaemon() {
  return new Promise((resolve) => {
    const child = spawn("paseo", ["reload"], { stdio: "ignore" });
    const timer = setTimeout(() => child.kill("SIGKILL"), RELOAD_TIMEOUT_MS);
    child.on("error", () => { clearTimeout(timer); resolve(false); });
    child.on("close", (code) => { clearTimeout(timer); resolve(code === 0); });
  });
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const file = process.env.BUZZ_PASEO_AGENTS_FILE ?? "/etc/paseo-buzz/agents/agents.json";
  const configFile = path.join(process.env.PASEO_HOME, "config.json");
  const intervalMs = 30_000;
  const log = (message) => process.stderr.write(`paseo-agents-watch: ${message}\n`);
  const readAgents = async () => {
    try { return await readFile(file, "utf8"); } catch { return null; }
  };
  const readConfig = async () => JSON.parse(await readFile(configFile, "utf8"));
  const writeConfig = async (config) => {
    const temporary = `${configFile}.agents-watch`;
    await writeFile(temporary, `${JSON.stringify(config, null, 2)}\n`, { mode: 0o600 });
    await rename(temporary, configFile);
  };
  // Startup already applied the environment's list.
  let state = { applied: (() => {
    try { return canonical(JSON.parse(process.env.BUZZ_PASEO_AGENTS || "[]")); } catch { return null; }
  })() };
  const restoreLog = (message) => process.stderr.write(`paseo-restore: ${message}\n`);
  const restorer = createRestorer({ pass: daemonPass(restoreLog), log: restoreLog });
  try {
    restorer.add((await buzzProvidersFromConfig(configFile)).map((provider) => provider.id));
  } catch (error) {
    restoreLog(`could not read the providers: ${String(error?.message ?? error).slice(0, 200)}`);
  }
  for (;;) {
    try {
      ({ state } = await watchOnce({ state, readAgents, readConfig, writeConfig, reload: reloadDaemon,
        owner: process.env.BUZZ_PASEO_OWNER ?? "", log, restorer }));
    } catch (error) {
      log(`check failed: ${String(error?.message ?? error).slice(0, 200)}`);
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}
