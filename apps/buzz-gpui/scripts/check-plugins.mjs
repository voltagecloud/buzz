// Checks a package's bundled plugins: starts each with the Node running
// this script, as Buzz would, and expects an answer to `initialize`.
//
// Usage: <bundled node> scripts/check-plugins.mjs <plugins folder>
import { spawn } from "node:child_process";
import { readdirSync, readFileSync, existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const folder = process.argv[2];
if (!folder) {
  console.error("Usage: node scripts/check-plugins.mjs <plugins folder>");
  process.exit(2);
}
const plugins = readdirSync(folder).filter((name) => existsSync(join(folder, name, "buzz-plugin.json")));
if (!plugins.length) {
  console.error(`No plugins in ${folder}`);
  process.exit(1);
}

function check(name) {
  const dir = join(folder, name);
  const manifest = JSON.parse(readFileSync(join(dir, "buzz-plugin.json"), "utf8"));
  const [program, ...args] = manifest.command;
  return new Promise((resolve) => {
    const child = spawn(program === "node" ? process.execPath : program, args, {
      cwd: dir,
      env: { ...process.env, BUZZ_PLUGIN_ID: manifest.id, BUZZ_PLUGIN_DATA_DIR: mkdtempSync(join(tmpdir(), "plugin-")) },
      stdio: ["pipe", "pipe", "inherit"],
    });
    const timer = setTimeout(() => finish(`${manifest.id}: no answer to initialize`), 10_000);
    let buffered = "";
    const finish = (error) => {
      clearTimeout(timer);
      child.kill();
      resolve(error);
    };
    child.on("error", (error) => finish(`${manifest.id}: ${error.message}`));
    child.stdout.on("data", (chunk) => {
      buffered += chunk;
      for (const line of buffered.split("\n").slice(0, -1)) {
        const message = JSON.parse(line);
        // The plugin's own requests (it may ask before it answers).
        if (message.method) continue;
        if (message.id === 1 && message.result) finish(undefined);
        else if (message.id === 1) finish(`${manifest.id}: ${JSON.stringify(message.error)}`);
      }
      buffered = buffered.slice(buffered.lastIndexOf("\n") + 1);
    });
    child.stdin.write(
      `${JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { apiVersion: 1, app: { name: "Buzz", version: "check" }, host: { browser: false } } })}\n`,
    );
  });
}

const errors = (await Promise.all(plugins.map(check))).filter(Boolean);
for (const error of errors) console.error(error);
console.log(`${plugins.length - errors.length} of ${plugins.length} plugins answered on Node ${process.version}`);
process.exit(errors.length ? 1 : 0);
