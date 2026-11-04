import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const check = fileURLToPath(new URL("check-remote-provider.mjs", import.meta.url));
const valid = {
  ok: true, name: "relay", version: "0.1.0", description: "Relay provider",
  protocol_version: 1, config_schema: {}, ops: ["info", "deploy", "stop", "recover"],
};

function run(info) {
  const directory = mkdtempSync(join(tmpdir(), "provider-package-check-"));
  try {
    const binary = join(directory, "provider");
    writeFileSync(binary, `#!${process.execPath}\nprocess.stdout.write(${JSON.stringify(JSON.stringify(info))});\n`, { mode: 0o755 });
    return spawnSync(process.execPath, [check, binary], {
      encoding: "utf8", timeout: 5_000, maxBuffer: 64 * 1024,
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test("accepts the complete desktop provider response", () => {
  const result = run(valid);
  assert.equal(result.status, 0, result.stderr);
});

test("rejects missing, empty, and wrongly typed desktop metadata", () => {
  for (const field of ["version", "description", "config_schema"]) {
    const values = field === "config_schema"
      ? [undefined, null, [], "schema", 1, false]
      : [undefined, null, "", 1, false, {}, []];
    for (const value of values) {
      const result = run({ ...valid, [field]: value });
      assert.equal(result.error, undefined);
      assert.notEqual(result.status, 0, `${field}=${JSON.stringify(value)} must fail`);
    }
  }
});

test("still requires the supported protocol and lifecycle operations", () => {
  for (const patch of [{ protocol_version: 2 }, { ops: ["info", "deploy"] }, { ops: "deploy stop recover" }]) {
    assert.notEqual(run({ ...valid, ...patch }).status, 0);
  }
});
