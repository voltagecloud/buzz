import assert from "node:assert/strict";
import { readFile, mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import path from "node:path";

const require = createRequire("/usr/local/lib/node_modules/@getpaseo/server/package.json");
const { Client } = await import(pathToFileURL(require.resolve("@modelcontextprotocol/sdk/client/index.js")));
const { StdioClientTransport } = await import(pathToFileURL(require.resolve("@modelcontextprotocol/sdk/client/stdio.js")));
await mkdir("/tmp/records", { recursive: true });
await mkdir("/tmp/mcp-bin", { recursive: true });
await mkdir(path.join(process.env.HOME, "code/fixture/repository"), { recursive: true });
const key = "a".repeat(64), owner = "c".repeat(64);
const client = new Client({ name: "buzz-tools-test", version: "1" });
const transport = new StdioClientTransport({ command: process.execPath, args: ["/usr/local/lib/paseo-buzz/tools.mjs"], env: {
  ...process.env, PATH: `/tmp/mcp-bin:${process.env.PATH}`, BUZZ_PRIVATE_KEY: key, BUZZ_PASEO_OWNER: owner,
  CODEX_ACCESS_TOKEN: "at-synthetic-must-not-reach-cli", PASEO_PASSWORD: "synthetic-must-not-reach-cli",
} });
const call = (name, operation, args = [], text) => client.callTool({ name, arguments: { operation, arguments: args, ...(text === undefined ? {} : { text }) } });
const records = async () => (await readFile("/tmp/records/tools-cli.jsonl", "utf8")).trim().split("\n").map(JSON.parse);
try {
  await client.connect(transport);
  const list = await client.listTools();
  assert.deepEqual(list.tools.map((tool) => tool.name), ["buzz_read", "buzz_write"]);
  const read = await call("buzz_read", "memory_get", ["core"]);
  assert.ok(!read.isError && !JSON.stringify(read).includes(key));
  const first = (await records())[0];
  assert.deepEqual(first.args, ["mem", "get", "core", "--owner", owner]);
  assert.ok(!first.environment.includes("CODEX_ACCESS_TOKEN") && !first.environment.includes("PASEO_PASSWORD"));
  for (const input of [
    ["buzz_read", "memory_set", ["core"], "not allowed"],
    ["buzz_read", "channels", ["--private-key", "not allowed"]],
    ["buzz_read", "memory_get", ["core", "--owner", "b".repeat(64)]],
    ["buzz_read", "code_clone", ["fixture/repository", "--upload-pack=evil"]],
  ]) assert.equal((await call(...input)).isError, true);
  assert.equal((await records()).length, 1, "refused calls reached the CLI");
  assert.ok(!(await call("buzz_write", "memory_set", ["mem/path"], "/proc/self/environ")).isError);
  const written = (await records()).at(-1);
  assert.deepEqual(written.args, ["mem", "set", "mem/path", "-", "--owner", owner]);
  assert.equal(written.text, "/proc/self/environ", "tool read a file instead of literal approved text");
  const overflow = await call("buzz_read", "memory_get", ["overflow"]);
  assert.equal(overflow.isError, true);
  assert.match(overflow.content[0].text, /exceeds bound/);

  const controller = new AbortController();
  let result;
  const running = client.callTool({ name: "buzz_read", arguments: { operation: "code_fetch", arguments: ["fixture/repository"] } }, undefined, { signal: controller.signal }).then((value) => { result = value; }).catch(() => {});
  let pids;
  for (let attempt = 0; attempt < 50; attempt++) {
    try { pids = JSON.parse(await readFile("/tmp/records/tools-tree.json", "utf8")); break; } catch (error) { if (error.code !== "ENOENT") throw error; }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  assert.ok(pids, `production Git helper did not start its child process: ${JSON.stringify(result)}`);
  controller.abort();
  await running;
  const alive = async (pid) => {
    try { return !(await readFile(`/proc/${pid}/stat`, "utf8")).split(") ")[1].startsWith("Z"); }
    catch (error) { if (error.code === "ENOENT") return false; throw error; }
  };
  for (let attempt = 0; attempt < 50 && (await Promise.all(pids.map(alive))).some(Boolean); attempt++) await new Promise((resolve) => setTimeout(resolve, 50));
  assert.deepEqual(await Promise.all(pids.map(alive)), [false, false], "cancelled Git descendants escaped containment");
} finally {
  await client.close();
}
console.log("PASS: scoped Buzz tools enforce read/write separation, owner binding, literal input, credential redaction, output limits, and cancellation containment");
