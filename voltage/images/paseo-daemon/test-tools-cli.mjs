#!/usr/bin/env node
// Synthetic CLI boundary for the MCP contract; never contacts a relay.
import fs from "node:fs";
import { spawn } from "node:child_process";
if (process.argv[1].endsWith("/git")) {
  const child = spawn(process.execPath, ["-e", "setInterval(()=>{},1000)"]);
  fs.writeFileSync("/tmp/records/tools-tree.json", JSON.stringify([process.pid, child.pid]));
  setInterval(() => {}, 1000);
} else {
const args = process.argv.slice(2);
const text = fs.readFileSync(0, "utf8");
fs.appendFileSync("/tmp/records/tools-cli.jsonl", `${JSON.stringify({ args, text, environment: Object.keys(process.env) })}\n`);
if (args.includes("overflow")) process.stdout.write("x".repeat(1024 * 1024));
else process.stdout.write(JSON.stringify({ accepted: true, credential: process.env.BUZZ_PRIVATE_KEY }));
}
