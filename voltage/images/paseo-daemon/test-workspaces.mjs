import assert from "node:assert/strict";
import { sync } from "./workspaces.mjs";

// A fake daemon that answers after two polls, one repository already
// checked out and registered, one cloneable, one the token cannot read, and
// the ~/buzz home of restored conversations.
const home = "/home/paseo";
const present = new Set([`${home}/code/voltagecloud/buzz`]);
const calls = [];
let polls = 0;
const exec = async (command, args) => {
  calls.push([command, ...args]);
  if (command === "paseo" && args[0] === "project" && args[1] === "ls") {
    polls += 1;
    if (polls < 3) return { code: 1, stdout: "" };
    return { code: 0, stdout: JSON.stringify([{ path: "/" }, { path: `${home}/code/voltagecloud/buzz` }]) };
  }
  if (command === "paseo-code" && args[0] === "clone") {
    if (args[1] === "voltagecloud/private") return { code: 1, stdout: "" };
    present.add(`${home}/code/${args[1]}`);
    return { code: 0, stdout: "" };
  }
  if (command === "paseo" && args[1] === "create") return { code: 0, stdout: "" };
  throw new Error(`unexpected ${command} ${args.join(" ")}`);
};
const logs = [];
const results = await sync({
  repos: ["voltagecloud/buzz", "voltagecloud/backend", "voltagecloud/private"], projects: [`${home}/buzz`],
  home, exec, log: (line) => logs.push(line), delayMs: 1, exists: (dir) => present.has(dir),
});
assert.deepEqual(results, [
  { project: `${home}/buzz`, state: "registered" },
  { repo: "voltagecloud/buzz", state: "registered" },
  { repo: "voltagecloud/backend", state: "registered" },
  { repo: "voltagecloud/private", state: "clone_failed" },
]);
// The existing checkout is neither cloned nor registered again. ~/buzz is
// registered before any clone, so the restore that waits for it does not
// wait for clones.
assert.ok(!calls.some((call) => call.join(" ") === "paseo-code clone voltagecloud/buzz"));
assert.deepEqual(calls.filter((call) => call[2] === "create"), [
  ["paseo", "project", "create", `${home}/buzz`],
  ["paseo", "project", "create", `${home}/code/voltagecloud/backend`],
]);
assert.ok(calls.findIndex((call) => call[2] === "create") < calls.findIndex((call) => call[0] === "paseo-code"), "~/buzz before clones");
assert.ok(logs.some((line) => line.includes("could not clone voltagecloud/private")));
assert.ok(!logs.join(" ").includes("github_pat"));

// No configured repositories: nothing runs.
calls.length = 0;
assert.deepEqual(await sync({ repos: [], home, exec }), []);
assert.equal(calls.length, 0);

// A daemon that never answers stops after the bounded attempts.
const silent = async () => ({ code: 1, stdout: "" });
const gaveUp = await sync({ repos: ["voltagecloud/buzz"], projects: [`${home}/buzz`], home, exec: silent, attempts: 3, delayMs: 1 });
assert.deepEqual(gaveUp, [{ project: `${home}/buzz`, state: "daemon_unavailable" }, { repo: "voltagecloud/buzz", state: "daemon_unavailable" }]);

// Without repositories, ~/buzz is still registered, and only once.
calls.length = 0;
polls = 3;
assert.deepEqual(await sync({ repos: [], projects: [`${home}/buzz`], home, exec, delayMs: 1 }), [{ project: `${home}/buzz`, state: "registered" }]);
assert.equal(calls.filter((call) => call[2] === "create").length, 1);
console.log("workspaces: ok");

// Production routing roots with real directories, agent records, and shared
// clones. Only external CLI/daemon discovery and clone transport are replaced.
const { syncWorkspaces, createSync, liveCwds } = await import("./workspaces.mjs");
const { mkdtemp, mkdir, writeFile, readFile, readdir, lstat, rm } = await import("node:fs/promises");
const { spawnSync } = await import("node:child_process");
const os = await import("node:os");
const path = await import("node:path");
const tmp = await mkdtemp(path.join(os.tmpdir(), "paseo-workspaces-"));
const owner = "a".repeat(64);
const channel = "6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e";
const ws = (slug, sources = []) => ({ owner, slug, name: slug, channel, sources: sources.map((repo) => `https://github.com/${repo}`), directory: slug, created_at: 1 });
const exists = (file) => lstat(file).then(() => true, () => false);
const git = (dir, ...args) => {
  const result = spawnSync("git", ["-C", dir, ...args], { encoding: "utf8", env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" } });
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
};
try {
  const home = tmp;
  const paseoHome = path.join(home, ".paseo");
  await mkdir(path.join(home, "buzz"));
  await mkdir(path.join(paseoHome, "agents", "records"), { recursive: true });
  const record = (id, cwd, archivedAt) => writeFile(path.join(paseoHome, "agents", "records", `${id}.json`), JSON.stringify({ cwd, archivedAt, provider: "brokkr" }));
  let selected = [ws("scratch"), ws("review-a", ["fixture/shared"]), ws("review-b", ["fixture/shared"])];
  let manifest = ["fixture/prefetch"];
  let readFails = false;
  let scopeOverride = null;
  let holdRead = null;
  const projects = new Set();
  const actions = [];
  const clone = async (repo) => {
    const dir = path.join(home, "code", repo);
    await mkdir(dir, { recursive: true });
    git(dir, "init", "-q");
    git(dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "initial");
    git(dir, "remote", "add", "origin", `https://github.com/${repo}.git`);
    git(dir, "update-ref", "refs/remotes/origin/main", "HEAD");
  };
  const execute = async (cmd, args) => {
    actions.push([cmd, ...args]);
    if (cmd === "buzz") {
      assert.deepEqual(args, ["paseo", "workspace", "list", "--owner", owner]);
      if (holdRead) await holdRead;
      return { code: readFails ? 1 : 0, stdout: JSON.stringify(selected) };
    }
    if (cmd === "paseo-code" && args[0] === "scope") return scopeOverride ?? { code: 0, stdout: JSON.stringify(manifest) };
    if (cmd === "paseo" && args[1] === "ls") return { code: 0, stdout: JSON.stringify([...projects].map((path) => ({ path }))) };
    if (cmd === "paseo" && args[1] === "create") { projects.add(args[2]); return { code: 0 }; }
    if (cmd === "paseo-code" && args[0] === "clone") { await clone(args[1]); return { code: 0 }; }
    throw new Error(`unexpected ${cmd} ${args}`);
  };
  const options = { owner, home, paseoHome, exec: execute, delayMs: 0, attempts: 1 };
  const tick = createSync(options);
  assert.equal((await tick()).state, "synced");
  const root = path.join(home, "workspaces");
  for (const slug of ["scratch", "review-a", "review-b"]) {
    assert.ok(projects.has(path.join(root, slug)));
    assert.deepEqual(await readdir(path.join(root, slug)), [], "routing root stays empty, even with sources");
  }
  assert.equal(await exists(path.join(home, "code", "fixture", "shared")), false, "workspace sources cause no background clones");
  const promptFile = path.join(paseoHome, "buzz-workspaces-prompt.txt");
  const prompt = await readFile(promptFile, "utf8");
  assert.ok(prompt.includes(channel) && prompt.includes("workspaceId"));
  assert.ok(prompt.includes("https://github.com/fixture/shared") && prompt.includes("paseo-code clone"));
  assert.ok(prompt.includes("~/code/owner/repository") && prompt.includes("Sources are context only"));

  // Existing manifest prefetch still clones and registers only its entries.
  assert.deepEqual(actions.filter(([cmd, arg]) => cmd === "paseo-code" && arg === "clone"), [["paseo-code", "clone", "fixture/prefetch"]]);
  assert.ok(projects.has(path.join(home, "code", "fixture", "prefetch")));
  await rm(path.join(home, "code", "fixture", "prefetch"), { recursive: true });
  actions.length = 0;
  await tick();
  assert.ok(!actions.some(([cmd, arg]) => cmd === "paseo-code" && arg === "clone"), "manifest prefetch stays one-time, as on main");
  await syncWorkspaces(options); // A new startup still performs manifest prefetch.
  // A source cloned on demand is reused without creating source views.
  await clone("fixture/shared");
  await tick();
  assert.ok(await exists(path.join(home, "code", "fixture", "shared")));
  assert.deepEqual(await readdir(path.join(root, "review-a")), []);

  // Removal retains directory and registration across resyncs. A clone whose
  // source context was removed stays available.
  selected = [ws("scratch")];
  await tick(); await tick();
  assert.ok(await exists(path.join(root, "review-a")));
  assert.ok(projects.has(path.join(root, "review-a")));
  assert.ok(await exists(path.join(home, "code", "fixture", "shared")));

  // Accept 32 active workspaces. Oversized CLI output changes nothing.
  selected = Array.from({ length: 32 }, (_, i) => ws(`active-${i}`));
  assert.equal((await tick()).workspaces, 32);
  const stateFile = path.join(paseoHome, "buzz-workspaces.json");
  const snapshot = await readFile(stateFile, "utf8");
  const entries = await readdir(root);
  selected.push(ws("overflow"));
  assert.equal((await tick()).state, "read_failed");
  assert.deepEqual(await readdir(root), entries);
  assert.equal(await readFile(stateFile, "utf8"), snapshot);

  // Forty live retired roots survive cleanup, independently of the cap of 32
  // unused retired routing directories.
  for (let i = 0; i < 40; i++) {
    await mkdir(path.join(root, `live-${i}`));
    await record(`root-${i}`, path.join(root, `live-${i}`));
  }
  selected = [];
  await tick(); await tick();
  for (let i = 0; i < 40; i++) assert.ok(await exists(path.join(root, `live-${i}`)), `live directory ${i}`);
  assert.equal((await readdir(root)).length, 40 + 32);

  // On-demand clones outside source context and the manifest stay until pod
  // replacement. The daemon runs no Git or worktree operations on them.
  await clone("fixture/on-demand");
  await tick(); await tick();
  assert.ok(await exists(path.join(home, "code", "fixture", "on-demand")));

  // Incomplete records disable cleanup. Archiving releases the cwd pin.
  await writeFile(path.join(paseoHome, "agents", "records", "bad.json"), "{");
  await mkdir(path.join(root, "extra"));
  const rootsWithIncompleteRecords = (await readdir(root)).sort();
  await tick();
  assert.deepEqual((await readdir(root)).sort(), rootsWithIncompleteRecords);
  await rm(path.join(paseoHome, "agents", "records", "bad.json"));
  await record("root-0", path.join(root, "live-0"), new Date().toISOString());
  const retirement = JSON.parse(await readFile(stateFile, "utf8"));
  retirement.retired["live-0"].retiredAt = 0;
  await writeFile(stateFile, JSON.stringify(retirement));
  await tick();
  assert.equal(await exists(path.join(root, "live-0")), false);

  // Failed reads change no registration, directories, prompt, or state.
  actions.length = 0;
  const beforeFailure = await readFile(stateFile, "utf8");
  const promptBeforeFailure = await readFile(promptFile, "utf8");
  const rootsBeforeFailure = await readdir(root);
  readFails = true;
  assert.equal((await tick()).state, "read_failed");
  assert.equal(actions.length, 1);
  assert.equal(await readFile(stateFile, "utf8"), beforeFailure);
  assert.equal(await readFile(promptFile, "utf8"), promptBeforeFailure);
  assert.deepEqual(await readdir(root), rootsBeforeFailure);
  readFails = false;
  for (const failure of [{ code: 1, stdout: "[]" }, { code: 0, stdout: "{" }]) {
    actions.length = 0;
    scopeOverride = failure;
    assert.equal((await tick()).state, "read_failed");
    assert.equal(actions.length, 2, "failed manifest starts no filesystem or daemon operations");
    assert.equal(await readFile(stateFile, "utf8"), beforeFailure);
    assert.equal(await readFile(promptFile, "utf8"), promptBeforeFailure);
    assert.deepEqual(await readdir(root), rootsBeforeFailure);
  }
  scopeOverride = null;
  let release;
  holdRead = new Promise((resolve) => { release = resolve; });
  const pending = tick();
  assert.deepEqual(await tick(), { state: "skipped" });
  release(); holdRead = null;
  assert.equal((await pending).state, "synced");
  assert.ok((await liveCwds(paseoHome, home)).length >= 40);
} finally { await rm(tmp, { recursive: true, force: true }); }
console.log("owner workspaces: ok");
