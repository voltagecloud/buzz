import assert from "node:assert/strict";
import { buzzProviders, withAgents } from "./providers.mjs";
import { checkOnce } from "./agents-watch.mjs";

const owner = "b".repeat(64);
const agent = (id, label, key) => ({ id, label, agent: key.repeat(64), channel: "6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e" });
const brokkr = agent("brokkr", "Brokkr", "a");
const tyr = agent("tyr", "Tyr", "c");

// The builder validates every field and keeps reserved ids out.
for (const [row, agents, message] of [
  ["not an array", {}, "JSON array"],
  ["bad id", [{ ...brokkr, id: "Bad_Id" }], ".id must match"],
  ["reserved", [{ ...brokkr, id: "claude" }], "reserved or duplicated"],
  ["duplicate", [brokkr, brokkr], "reserved or duplicated"],
  ["bad key", [{ ...brokkr, agent: "x" }], ".agent must be"],
  ["bad channel", [{ ...brokkr, channel: "x" }], ".channel must be a UUID"],
]) {
  assert.throws(() => buzzProviders(agents, owner, ["claude"]), (error) => error.message.includes(message), row);
}
assert.throws(() => buzzProviders([brokkr], "", []), /OWNER is required/);
const built = buzzProviders([brokkr], owner, ["claude"]);
assert.deepEqual(built.brokkr.command, ["buzz", "acp-bridge", "--agent", brokkr.agent, "--channel", brokkr.channel, "--owner", owner]);
assert.deepEqual(built.brokkr.models, [{ id: "buzz", label: "Brokkr", isDefault: true }]);

// Replacing the list keeps every other provider and replaces only Buzz ones.
const config = { daemon: { x: 1 }, agents: { providers: { claude: { enabled: true, label: "PA" }, codex: { enabled: false }, ...built } } };
const swapped = withAgents(config, [tyr], owner);
assert.deepEqual(Object.keys(swapped.agents.providers).sort(), ["claude", "codex", "tyr"]);
assert.equal(swapped.agents.providers.claude.label, "PA");
assert.deepEqual(swapped.daemon, { x: 1 });

// The watcher: unchanged lists do nothing, a new list rewrites and reloads,
// an invalid list or failed reload keeps the state for a retry.
let written = null;
let reloads = 0;
let reloadOk = true;
const logs = [];
const deps = (text) => ({
  readAgents: async () => text,
  readConfig: async () => structuredClone(config),
  writeConfig: async (next) => { written = next; },
  reload: async () => { reloads += 1; return reloadOk; },
  owner,
  log: (line) => logs.push(line),
});
let state = { applied: JSON.stringify([brokkr]) };
let result = await checkOnce({ state, ...deps(null) });
assert.equal(result.outcome, "absent");
result = await checkOnce({ state, ...deps(JSON.stringify([brokkr], null, 2)) });
assert.equal(result.outcome, "unchanged", "formatting alone does not reload");
assert.equal(reloads, 0);
result = await checkOnce({ state, ...deps(JSON.stringify([brokkr, tyr])) });
assert.equal(result.outcome, "reloaded");
assert.deepEqual(Object.keys(written.agents.providers).sort(), ["brokkr", "claude", "codex", "tyr"]);
assert.equal(reloads, 1);
state = result.state;
written = null;
result = await checkOnce({ state, ...deps(JSON.stringify([{ ...tyr, id: "claude" }])) });
assert.equal(result.outcome, "invalid");
assert.equal(written, null, "an invalid list never rewrites the config");
result = await checkOnce({ state, ...deps("{not json") });
assert.equal(result.outcome, "invalid");
reloadOk = false;
result = await checkOnce({ state, ...deps(JSON.stringify([tyr])) });
assert.equal(result.outcome, "reload_failed");
assert.equal(result.state, state, "a failed reload is retried on the next check");
assert.ok(!logs.join(" ").includes(owner));
console.log("agents-watch: ok");
