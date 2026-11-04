// Exercise the packaged provider without credentials or a relay connection.
import { spawnSync } from "node:child_process";

const binary = process.argv[2];
if (!binary) throw new Error("Expected the packaged buzz-backend-relay path");
const result = spawnSync(binary, {
  input: '{"op":"info"}\n',
  encoding: "utf8",
  timeout: 10_000,
  maxBuffer: 64 * 1024,
});
if (result.error || result.status !== 0) {
  throw new Error("The packaged relay provider could not start");
}
const info = JSON.parse(result.stdout);
const nonemptyText = (field) => typeof info?.[field] === "string" && info[field].length > 0;
if (info?.ok !== true || info.name !== "relay" || info.protocol_version !== 1 ||
    !nonemptyText("version") || !nonemptyText("description") ||
    !info.config_schema || typeof info.config_schema !== "object" || Array.isArray(info.config_schema) ||
    !Array.isArray(info.ops) || !["deploy", "stop", "recover"].every((op) => info.ops.includes(op))) {
  throw new Error("The packaged relay provider has an incompatible protocol");
}
console.log("Packaged relay provider supports deploy, stop, and recovery");
