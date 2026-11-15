import assert from "node:assert/strict";
import { buzzProviders, withAgents } from "./providers.mjs";
import { checkOnce, watchOnce } from "./agents-watch.mjs";
import { createRestorer } from "./restore.mjs";

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
assert.deepEqual(result.added, ["tyr"], "only the new provider's conversations are restored");
assert.deepEqual(Object.keys(written.agents.providers).sort(), ["brokkr", "claude", "codex", "tyr"]);
assert.equal(reloads, 1);
state = result.state;
written = null;
result = await checkOnce({ state, ...deps(JSON.stringify([{ ...tyr, id: "claude" }])) });
assert.equal(result.outcome, "invalid");
assert.equal(written, null, "an invalid list never rewrites the config");
result = await checkOnce({ state, ...deps("{not json") });
assert.equal(result.outcome, "invalid");
const moved = { ...brokkr, channel: "0f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e" };
result = await checkOnce({ state, ...deps(JSON.stringify([moved, tyr])) });
assert.equal(result.outcome, "reloaded");
assert.deepEqual(result.added, [], "a changed provider is not restored: Paseo already has its conversations");
reloadOk = false;
result = await checkOnce({ state, ...deps(JSON.stringify([tyr])) });
assert.equal(result.outcome, "reload_failed");
assert.equal(result.state, state, "a failed reload is retried on the next check");
assert.ok(!logs.join(" ").includes(owner));

// The loop iteration hands only added providers to the restorer, and starts
// its attempts even when the check fails.
reloadOk = true;
const passes = [];
const restorer = createRestorer({ pass: async (tasks) => { passes.push(tasks); return new Map(); } });
// Lets a started pass finish without starting one.
const settle = () => new Promise((resolve) => setImmediate(resolve));
result = await watchOnce({ state: { applied: JSON.stringify([brokkr]) }, ...deps(JSON.stringify([brokkr, tyr])), restorer });
assert.equal(result.outcome, "reloaded");
await settle();
assert.deepEqual(passes, [[{ id: "tyr", sessions: null }]], "the added provider is restored");
await watchOnce({ state: result.state, ...deps(JSON.stringify([moved, tyr])), restorer });
await settle();
assert.equal(passes.length, 1, "a changed provider is not restored");
restorer.add(["brokkr"]);
await assert.rejects(watchOnce({ state, ...deps(null), readAgents: async () => { throw new Error("read failed"); }, restorer }));
await settle();
assert.deepEqual(passes[1], [{ id: "brokkr", sessions: null }], "a failed check still starts due restores");
// Channel-less entries work at startup and on reload, with no --channel argv.
const channelLess = { ...tyr };
delete channelLess.channel;
assert.deepEqual(buzzProviders([channelLess], owner, []).tyr.command,
  ["buzz", "acp-bridge", "--agent", tyr.agent, "--owner", owner]);
assert.throws(() => buzzProviders([{ ...tyr, channel: null }], owner, []), /channel must be a UUID/);
const generated = { ...config, daemon: { appendSystemPrompt:
  "<!-- BUZZ_AGENTS_BEGIN -->old agents<!-- BUZZ_AGENTS_END -->\n<!-- BUZZ_WORKSPACES_BEGIN -->old workspaces<!-- BUZZ_WORKSPACES_END -->" } };
const generatedDeps = { ...deps(JSON.stringify([channelLess])), readConfig: async () => generated,
  readWorkspacePrompt: async () => "scratch: channel example; pass workspaceId" };
result = await checkOnce({ state: { applied: JSON.stringify([brokkr]) }, ...generatedDeps });
assert.equal(result.outcome, "reloaded");
assert.ok(written.daemon.appendSystemPrompt.includes("scratch: channel example"));
assert.ok(written.daemon.appendSystemPrompt.includes('"Tyr"'));
assert.ok(!written.daemon.appendSystemPrompt.includes("undefined"));
const changedState = result.state;
result = await checkOnce({ state: changedState, ...generatedDeps });
assert.equal(result.outcome, "unchanged");
result = await checkOnce({ state: changedState, ...generatedDeps,
  readAgents: async () => null, readWorkspacePrompt: async () => "updated workspace channel" });
assert.equal(result.outcome, "reloaded", "prompt refresh needs no agent-list mount");
assert.ok(written.daemon.appendSystemPrompt.includes("updated workspace channel"));
assert.deepEqual(result.added, []);
const keyId = { ...channelLess, id: "1".repeat(64), label: "<!-- BUZZ_WORKSPACES_END -->" };
assert.ok(buzzProviders([keyId], owner, [])[keyId.id]);
result = await checkOnce({ state: changedState, ...generatedDeps, readAgents: async () => JSON.stringify([keyId]),
  readWorkspacePrompt: async () => { throw new Error("unreadable prompt"); } });
assert.equal(result.outcome, "reloaded", "bad workspace prompt does not block provider reload");
assert.ok(written.agents.providers[keyId.id]);
assert.equal(written.daemon.appendSystemPrompt.split("<!-- BUZZ_WORKSPACES_END -->").length, 2, "labels cannot inject section markers");
console.log("agents-watch: ok");
