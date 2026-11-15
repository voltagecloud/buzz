#!/usr/bin/env node
// Register empty routing roots for the Rust CLI's owner workspaces. Never interpret relay
// events here: the CLI owns signatures, channel policy, ordering, caps and names.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { lstat, mkdir, readFile, readdir, realpath, rename, rmdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const RUN_TIMEOUT_MS = 5 * 60_000;
const MAX_OUTPUT = 1024 * 1024;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Capture bounded output and kill the entire helper process group at exit. */
export function run(command, args, timeoutMs = RUN_TIMEOUT_MS) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { detached: true, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "", bytes = 0, failed = false;
    const kill = () => { try { process.kill(-child.pid, "SIGKILL"); } catch { /* Already gone. */ } };
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      bytes += Buffer.byteLength(chunk);
      if (bytes > MAX_OUTPUT) { failed = true; kill(); } else stdout += chunk;
    });
    child.stderr.resume();
    const timer = setTimeout(() => { failed = true; kill(); }, timeoutMs);
    child.on("error", () => { clearTimeout(timer); resolve({ code: -1, stdout: "" }); });
    child.on("close", (code) => { clearTimeout(timer); kill(); resolve({ code: failed ? -1 : code, stdout }); });
  });
}

/** Register directories before cloning manifest repositories (legacy projects). */
export async function sync({ repos, projects: directories = [], home, exec = run, log = () => {}, attempts = 60, delayMs = 2000, exists = existsSync }) {
  if (repos.length === 0 && directories.length === 0) return [];
  let projects = null;
  for (let attempt = 0; attempt < attempts && projects === null; attempt++) {
    const listed = await exec("paseo", ["project", "ls", "--json"]);
    if (listed.code === 0) {
      try { const parsed = JSON.parse(listed.stdout); if (Array.isArray(parsed)) projects = parsed; } catch { /* Retry readiness. */ }
    }
    if (projects === null) await sleep(delayMs);
  }
  if (projects === null) {
    log("the daemon did not answer; projects were not registered");
    return [...directories.map((project) => ({ project, state: "daemon_unavailable" })), ...repos.map((repo) => ({ repo, state: "daemon_unavailable" }))];
  }
  const registered = new Set(projects.map((project) => path.resolve(String(project.path ?? ""))));
  const register = async (directory) => {
    if (registered.has(path.resolve(directory))) return "registered";
    if ((await exec("paseo", ["project", "create", directory])).code !== 0) return "register_failed";
    registered.add(path.resolve(directory));
    return "registered";
  };
  const results = [];
  for (const project of directories) results.push({ project, state: await register(project) });
  for (const repo of repos) {
    const directory = path.join(home, "code", repo);
    if (!exists(directory)) {
      const cloned = await exec("paseo-code", ["clone", repo]);
      if (cloned.code !== 0 || !exists(directory)) {
        log(`could not clone ${repo}; check the GitHub read token permissions`);
        results.push({ repo, state: "clone_failed" });
        continue;
      }
    }
    results.push({ repo, state: await register(directory) });
  }
  return results;
}

const inside = (root, target) => target === root || target.startsWith(`${root}${path.sep}`);
async function directory(target) {
  await mkdir(target, { recursive: true, mode: 0o700 });
  if (await realpath(target) !== target || !(await lstat(target)).isDirectory()) throw new Error("managed directory must not be a symlink");
}
const atomic = async (file, text) => {
  await writeFile(`${file}.tmp`, text, { mode: 0o600 });
  await rename(`${file}.tmp`, file);
};

/** Exhaustive live cwd snapshot. An unreadable/oversized record disables cleanup. */
export async function liveCwds(paseoHome, home) {
  const found = [];
  let count = 0;
  const scan = async (dir, depth) => {
    let entries;
    try { entries = await readdir(dir, { withFileTypes: true }); }
    catch (error) { if (depth === 0 && error.code === "ENOENT") return; throw error; }
    for (const entry of entries) {
      if (++count > 10_000) throw new Error("agent record scan exceeded its bound");
      const file = path.join(dir, entry.name);
      if (entry.isDirectory() && depth === 0) await scan(file, 1);
      else if (entry.name.endsWith(".json")) {
        const info = await lstat(file);
        if (!info.isFile() || info.size > MAX_OUTPUT) throw new Error("unreadable agent record");
        const record = JSON.parse(await readFile(file, "utf8"));
        if (record.archivedAt) continue;
        if (typeof record.cwd !== "string" || !record.cwd) throw new Error("live agent has no cwd");
        const cwd = record.cwd.startsWith("~/") ? path.join(home, record.cwd.slice(2)) : record.cwd;
        if (!path.isAbsolute(cwd)) throw new Error("live agent cwd is not absolute");
        found.push(path.resolve(cwd));
        try { found.push(await realpath(cwd)); } catch { /* Lexical path still pins a missing cwd. */ }
      } else if (entry.isDirectory() || entry.isSymbolicLink()) throw new Error("unknown agent record layout");
    }
  };
  await scan(path.join(paseoHome, "agents"), 0);
  return found;
}

/** Register routing roots and refresh source context. A failed read changes nothing. */
export async function syncWorkspaces({ owner, home, paseoHome = path.join(home, ".paseo"), exec = run, log = () => {}, attempts = 60, delayMs = 2000, prefetch = true }) {
  let workspaces, repos;
  try {
    const listed = owner ? await exec("buzz", ["paseo", "workspace", "list", "--owner", owner]) : { code: 0, stdout: "[]" };
    if (listed.code !== 0) throw new Error("workspace list failed");
    workspaces = JSON.parse(listed.stdout);
    // Transport sanity only; selection, caps and slug mapping belong to Rust.
    if (!Array.isArray(workspaces) || workspaces.length > 32) throw new Error("invalid workspace list output");
    for (const ws of workspaces) {
      if (typeof ws.directory !== "string" || !ws.directory || ws.directory === "." || ws.directory === ".." || path.basename(ws.directory) !== ws.directory ||
          !Array.isArray(ws.sources)) throw new Error("invalid workspace list output");
    }
    const scope = await exec("paseo-code", ["scope"]);
    if (scope.code !== 0) throw new Error("repository manifest read failed");
    repos = JSON.parse(scope.stdout);
    if (!Array.isArray(repos)) throw new Error("invalid repository manifest");
  } catch {
    log("workspace or manifest read failed; kept all previous state");
    return { state: "read_failed" };
  }
  home = await realpath(home);
  const root = path.join(home, "workspaces");
  await directory(root);
  await directory(paseoHome);
  const stateFile = path.join(paseoHome, "buzz-workspaces.json");
  let previous = {};
  try { previous = JSON.parse(await readFile(stateFile, "utf8")); } catch (error) { if (error.code !== "ENOENT") throw error; }
  const active = new Set(workspaces.map((ws) => ws.directory));
  for (const ws of workspaces) await directory(path.join(root, ws.directory));
  const results = await sync({ repos: [], projects: [path.join(home, "buzz"), ...workspaces.map((ws) => path.join(root, ws.directory))], home, exec, log, attempts, delayMs });
  if (results.some((result) => result.state !== "registered")) return { state: "register_failed" };
  // Workspace sources are prompt context, never clone or worktree requests.
  let live = null;
  try { live = await liveCwds(paseoHome, home); } catch { log("agent records incomplete; skipped all cleanup"); }
  const retained = {};
  const unused = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (!entry.isDirectory() || active.has(entry.name)) continue;
    const dir = path.join(root, entry.name);
    const retiredAt = previous.retired?.[entry.name]?.retiredAt ?? Date.now();
    retained[entry.name] = { retiredAt };
    log(`retained retired workspace ${entry.name}`);
    if (live !== null && !live.some((cwd) => inside(dir, cwd))) unused.push({ dir, name: entry.name, retiredAt });
  }
  unused.sort((a, b) => a.retiredAt - b.retiredAt || a.name.localeCompare(b.name));
  for (const retired of unused.slice(0, Math.max(0, unused.length - 32))) {
    const current = await liveCwds(paseoHome, home);
    if (current.some((cwd) => inside(retired.dir, cwd))) continue;
    // Routing roots are empty. Refuse to remove unexpected contents.
    try { await rmdir(retired.dir); }
    catch (error) { if (error.code !== "ENOTEMPTY" && error.code !== "EEXIST") throw error; continue; }
    delete retained[retired.name];
  }
  // On-demand clones are first-class: shared clones under ~/code last until pod replacement.
  await atomic(stateFile, `${JSON.stringify({ retired: retained, directories: [...active] })}\n`);
  // Agent watcher is the only config writer. This snapshot refreshes its prompt.
  await atomic(path.join(paseoHome, "buzz-workspaces-prompt.txt"), workspacePrompt(workspaces));
  // Preserve main's one-time manifest prefetch, independent of workspace sources.
  if (prefetch) await sync({ repos, home, exec, log, attempts: 1, delayMs: 0 });
  return { state: "synced", workspaces: workspaces.length };
}

/** Display only; workspace authority remains entirely in the bridge. */
export function workspacePrompt(workspaces) {
  const display = (value) => JSON.stringify(value).replaceAll("<", "\\u003c");
  return ["Choose the workspace with the intended Buzz channel. Use list_workspaces and pass its workspaceId to create_agent.",
    "Workspace directories are empty routing roots. Sources are context only; use paseo-code clone owner/repository on demand, then Read, Glob, and Grep under ~/code/owner/repository.",
    ...workspaces.map((ws) => `- ${display(ws.slug)}: channel ${ws.channel}, directory ~/workspaces/${ws.directory}, sources ${display(ws.sources)}`)].join("\n");
}

/** A slow run skips subsequent ticks, rather than overlapping filesystem edits. */
export function createSync(options) {
  let running = false;
  let prefetch = true;
  return async () => {
    if (running) return { state: "skipped" };
    running = true;
    try {
      const result = await syncWorkspaces({ ...options, prefetch });
      if (result.state === "synced") prefetch = false;
      return result;
    } finally { running = false; }
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const log = (message) => process.stderr.write(`paseo-workspaces: ${message}\n`);
  const tick = createSync({ owner: process.env.BUZZ_PASEO_OWNER, home: process.env.HOME, paseoHome: process.env.PASEO_HOME, log });
  const check = () => tick().catch(() => log("sync failed; retrying on the next tick"));
  setInterval(check, 60_000);
  await check();
}
