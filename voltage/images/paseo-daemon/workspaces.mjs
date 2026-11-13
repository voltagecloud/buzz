#!/usr/bin/env node
// Clone each repository the daemon's Git read token covers and register it
// as a Paseo project, so a paired app shows file trees without manual setup.
// Runs once in the background at startup. Idempotent: an existing checkout is
// not cloned again and a registered project is not registered again. A
// repository the token cannot read is logged and skipped. Never prints tokens.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** Bound on one helper process; `paseo-code clone` has its own deadline. */
const RUN_TIMEOUT_MS = 5 * 60_000;
const MAX_OUTPUT = 1024 * 1024;

/** Run a command, capturing at most MAX_OUTPUT of stdout, killed at the deadline. */
export function run(command, args, timeoutMs = RUN_TIMEOUT_MS) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    child.stdout.on("data", (chunk) => {
      if (stdout.length < MAX_OUTPUT) stdout += chunk;
    });
    child.stderr.resume();
    const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
    child.on("error", () => { clearTimeout(timer); resolve({ code: -1, stdout }); });
    child.on("close", (code) => { clearTimeout(timer); resolve({ code, stdout }); });
  });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Clone and register `repos` (normalized `owner/name`) under `home/code`.
 * Waits for the daemon by polling `paseo project ls`, at most `attempts`
 * times. Returns one `{repo, state}` per repository.
 */
export async function sync({ repos, home, exec = run, log = () => {}, attempts = 60, delayMs = 2000, exists = existsSync }) {
  if (repos.length === 0) return [];
  let projects = null;
  for (let attempt = 0; attempt < attempts && projects === null; attempt++) {
    const listed = await exec("paseo", ["project", "ls", "--json"]);
    if (listed.code === 0) {
      try {
        const parsed = JSON.parse(listed.stdout);
        if (Array.isArray(parsed)) projects = parsed;
      } catch { /* Not ready; retry. */ }
    }
    if (projects === null) await sleep(delayMs);
  }
  if (projects === null) {
    log("the daemon did not answer; repositories were not registered as projects");
    return repos.map((repo) => ({ repo, state: "daemon_unavailable" }));
  }
  const registered = new Set(projects.map((project) => path.resolve(String(project.path ?? ""))));
  const results = [];
  for (const repo of repos) {
    const directory = path.join(home, "code", repo);
    if (!exists(directory)) {
      const cloned = await exec("paseo-code", ["clone", repo]);
      if (cloned.code !== 0 || !exists(directory)) {
        log(`could not clone ${repo}; check that the Paseo GitHub read token can read it`);
        results.push({ repo, state: "clone_failed" });
        continue;
      }
    }
    if (registered.has(path.resolve(directory))) {
      results.push({ repo, state: "registered" });
      continue;
    }
    const created = await exec("paseo", ["project", "create", directory]);
    if (created.code !== 0) {
      log(`could not register ${repo} as a Paseo project`);
      results.push({ repo, state: "register_failed" });
      continue;
    }
    registered.add(path.resolve(directory));
    log(`registered ${repo}`);
    results.push({ repo, state: "registered" });
  }
  return results;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const scope = await run("paseo-code", ["scope"]);
  let repos = [];
  try {
    repos = scope.code === 0 ? JSON.parse(scope.stdout) : [];
  } catch { repos = []; }
  const log = (message) => process.stderr.write(`paseo-workspaces: ${message}\n`);
  await sync({ repos, home: process.env.HOME, log });
}
