import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import contribute, { buzz, createPublisher, gatewayName, publishOnce } from "./profile/server/profile.mjs";

const owner = "a".repeat(64);
const gateway = "b".repeat(64);

// A relay with an owner profile and the gateway's own profile.
function relay({ ownerName = "Taylor", gatewayName: current, failures = 0 } = {}) {
  const calls = [];
  let remaining = failures;
  const run = async (args) => {
    calls.push(args);
    if (remaining > 0) { remaining -= 1; throw new Error("relay unavailable"); }
    const command = args.join(" ");
    if (command === `--format compact users get --pubkey ${owner}`) {
      return JSON.stringify(ownerName === null ? [] : [{ pubkey: owner, display_name: ownerName }]);
    }
    if (command === "--format compact users get") {
      return JSON.stringify(current === undefined ? [] : [{ pubkey: gateway, display_name: current }]);
    }
    if (args[0] === "users" && args[1] === "set-profile") { current = args[args.indexOf("--name") + 1]; return ""; }
    throw new Error(`unexpected command: ${command}`);
  };
  return { run, calls, current: () => current };
}
const sets = (calls) => calls.filter((args) => args[1] === "set-profile");

// Names: the owner's name plus the source, cleaned and bounded.
assert.equal(gatewayName("Taylor"), "Taylor (Paseo)");
assert.equal(gatewayName("  Taylor \n Smith "), "Taylor Smith (Paseo)");
assert.equal(gatewayName("Tay‮lor\u0007"), "Taylor (Paseo)", "control and bidi characters are removed");
for (const missing of [undefined, null, "", "   ", "​", 42]) assert.equal(gatewayName(missing), undefined);
const long = gatewayName("x".repeat(200));
assert.equal([...long].length, 64);
assert.ok(long.endsWith("… (Paseo)"));

// The first attempt publishes the derived name and an explanatory about line.
{
  const network = relay();
  assert.equal(await publishOnce(owner, { run: network.run }), "published");
  assert.equal(network.current(), "Taylor (Paseo)");
  const [set] = sets(network.calls);
  assert.match(set[set.indexOf("--about") + 1], new RegExp(`Paseo gateway for ${owner}`));
}

// A restart with the current name writes nothing.
{
  const network = relay({ gatewayName: "Taylor (Paseo)" });
  assert.equal(await publishOnce(owner, { run: network.run }), "unchanged");
  assert.equal(sets(network.calls).length, 0);
}

// A renamed owner renames the gateway on the next start.
{
  const network = relay({ ownerName: "Tay", gatewayName: "Taylor (Paseo)" });
  assert.equal(await publishOnce(owner, { run: network.run }), "published");
  assert.equal(network.current(), "Tay (Paseo)");
}

// No owner profile: nothing is published, and the attempt fails so it retries.
{
  const network = relay({ ownerName: null });
  await assert.rejects(publishOnce(owner, { run: network.run }), /no name yet/);
  assert.equal(sets(network.calls).length, 0);
}

// Another author's profile in the reply never names the gateway.
{
  const run = async (args) => args.includes("--pubkey")
    ? JSON.stringify([{ pubkey: gateway, display_name: "Mallory" }]) : "[]";
  await assert.rejects(publishOnce(owner, { run }), /no name yet/);
}

// Failures retry with exponential backoff, then publish.
{
  const network = relay({ failures: 2 });
  const publisher = createPublisher(owner, { run: network.run, retryMs: 5, maxRetryMs: 20 });
  assert.equal(await publisher.finished, "published");
  assert.equal(network.current(), "Taylor (Paseo)");
}

// Retrying stops at the end of the retry window.
{
  let clock = 0;
  const run = async () => { clock += 1000; throw new Error("not a relay member"); };
  const publisher = createPublisher(owner, { run, retryMs: 1, maxRetryMs: 1, windowMs: 3000, now: () => clock });
  assert.equal(await publisher.finished, "gave-up");
}

// Closing stops a pending retry and any running command.
{
  let aborted = false;
  const run = (_args, { signal }) => new Promise((_resolve, reject) => {
    signal.addEventListener("abort", () => { aborted = true; reject(new Error("stopped")); }, { once: true });
  });
  const publisher = createPublisher(owner, { run, retryMs: 60000 });
  assert.equal(await publisher.close(), "stopped");
  assert.ok(aborted, "close() aborts the running command");
}

// Without an owner the plugin does nothing.
{
  const previous = process.env.BUZZ_PASEO_OWNER;
  delete process.env.BUZZ_PASEO_OWNER;
  const cleanup = contribute();
  assert.equal(typeof cleanup, "function");
  await cleanup();
  if (previous !== undefined) process.env.BUZZ_PASEO_OWNER = previous;
}

console.log("profile plugin tests passed");

// The real runner never passes BUZZ_AUTH_TAG to `buzz`: an `auth` tag on the
// gateway's own kind 0 would make it refuse to act as a gateway.
{
  const bin = mkdtempSync(path.join(tmpdir(), "buzz-profile-bin-"));
  writeFileSync(path.join(bin, "buzz"), '#!/bin/sh\nprintf "%s" "${BUZZ_AUTH_TAG-unset}"\n');
  chmodSync(path.join(bin, "buzz"), 0o755);
  const saved = { PATH: process.env.PATH, BUZZ_AUTH_TAG: process.env.BUZZ_AUTH_TAG };
  process.env.PATH = `${bin}:${saved.PATH}`;
  process.env.BUZZ_AUTH_TAG = '["auth","owner","","sig"]';
  try {
    assert.equal(await buzz(["users", "get"]), "unset");
  } finally {
    process.env.PATH = saved.PATH;
    if (saved.BUZZ_AUTH_TAG === undefined) delete process.env.BUZZ_AUTH_TAG;
    else process.env.BUZZ_AUTH_TAG = saved.BUZZ_AUTH_TAG;
  }
}
