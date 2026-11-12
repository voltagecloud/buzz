#!/usr/bin/env node
// Keep the daemon's Buzz remote-agent providers in step with the manager's
// agent list without restarting the daemon. Kubernetes refreshes the mounted
// copy of the daemon Secret's BUZZ_PASEO_AGENTS key after the manager changes
// it; this rewrites config.json's Buzz providers and runs `paseo reload`.
// An absent file (no mount) does nothing. An invalid list is logged and the
// current providers stay. Never prints secret values.
import { spawn } from "node:child_process";
import { readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { withAgents } from "./providers.mjs";

const MAX_FILE_BYTES = 256 * 1024;
const RELOAD_TIMEOUT_MS = 60_000;

/** The canonical text of an agent list, so formatting differences do not reload. */
const canonical = (agents) => JSON.stringify(agents);

/**
 * One check. `state.applied` is the canonical list last applied. Returns the
 * new state and what happened: "absent", "unchanged", "invalid", "reload_failed"
 * or "reloaded".
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
  log(`reloaded ${Array.isArray(agents) ? agents.length : 0} Buzz agent provider(s)`);
  return { state: { applied: canonical(agents) }, outcome: "reloaded" };
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
  for (;;) {
    try {
      ({ state } = await checkOnce({ state, readAgents, readConfig, writeConfig, reload: reloadDaemon,
        owner: process.env.BUZZ_PASEO_OWNER ?? "", log }));
    } catch (error) {
      log(`check failed: ${String(error?.message ?? error).slice(0, 200)}`);
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}
