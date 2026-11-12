import assert from "node:assert/strict";
import { sync } from "./workspaces.mjs";

// A fake daemon that answers after two polls, one repository already
// checked out and registered, one cloneable, and one the token cannot read.
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
  repos: ["voltagecloud/buzz", "voltagecloud/backend", "voltagecloud/private"],
  home, exec, log: (line) => logs.push(line), delayMs: 1, exists: (dir) => present.has(dir),
});
assert.deepEqual(results, [
  { repo: "voltagecloud/buzz", state: "registered" },
  { repo: "voltagecloud/backend", state: "registered" },
  { repo: "voltagecloud/private", state: "clone_failed" },
]);
// The existing checkout is neither cloned nor registered again.
assert.ok(!calls.some((call) => call.join(" ") === "paseo-code clone voltagecloud/buzz"));
assert.deepEqual(calls.filter((call) => call[2] === "create"), [["paseo", "project", "create", `${home}/code/voltagecloud/backend`]]);
assert.ok(logs.some((line) => line.includes("could not clone voltagecloud/private")));
assert.ok(!logs.join(" ").includes("github_pat"));

// No configured repositories: nothing runs.
calls.length = 0;
assert.deepEqual(await sync({ repos: [], home, exec }), []);
assert.equal(calls.length, 0);

// A daemon that never answers stops after the bounded attempts.
const silent = async () => ({ code: 1, stdout: "" });
const gaveUp = await sync({ repos: ["voltagecloud/buzz"], home, exec: silent, attempts: 3, delayMs: 1 });
assert.deepEqual(gaveUp, [{ repo: "voltagecloud/buzz", state: "daemon_unavailable" }]);
console.log("workspaces: ok");
