// Give the gateway key its own Buzz profile name, "<owner name> (Paseo)".
// Without a profile, Buzz shows the gateway's posts under a pubkey prefix.
// The name is the gateway's own kind 0 and claims no owner authority.
import { spawn } from "node:child_process";

const HEX_PUBKEY = /^[0-9a-f]{64}$/;
const SUFFIX = " (Paseo)";
const MAX_NAME_CHARS = 64;
const COMMAND_TIMEOUT_MS = 15000;
const MAX_OUTPUT_BYTES = 65536;
const RETRY_MS = 60000;
const MAX_RETRY_MS = 60 * 60 * 1000;
/// Retry this long, like the recorder: an operator can admit the gateway as a
/// relay member after the daemon starts. A restart retries again.
const RETRY_WINDOW_MS = 24 * 60 * 60 * 1000;
const log = (message) => console.error(`buzz-profile: ${message}`);

/// The gateway's display name, or undefined when the owner has no usable name.
export function gatewayName(ownerName) {
  if (typeof ownerName !== "string") return undefined;
  // Control and bidi characters would let a name render as something else.
  const name = ownerName.replace(/[\p{Cc}\p{Cf}]/gu, "").replace(/\s+/g, " ").trim();
  if (!name) return undefined;
  const budget = MAX_NAME_CHARS - SUFFIX.length;
  const chars = [...name];
  // A truncated name keeps one character of the budget for the ellipsis.
  return `${chars.length > budget ? `${chars.slice(0, budget - 1).join("").trimEnd()}…` : name}${SUFFIX}`;
}

/// Run `buzz` with a deadline and bounded output; resolve its stdout.
export function buzz(args, { signal } = {}) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error("stopped")); return; }
    // An inherited BUZZ_AUTH_TAG would sign the gateway's own kind 0 with an
    // `auth` tag, and a gateway whose profile claims an agent owner is refused.
    const { BUZZ_AUTH_TAG: _authTag, ...env } = process.env;
    const child = spawn("buzz", args, { stdio: ["ignore", "pipe", "ignore"], detached: true, env });
    let output = "";
    let overflow = false;
    const kill = () => { try { process.kill(-child.pid, "SIGKILL"); } catch { /* Already exited. */ } };
    const timeout = setTimeout(kill, COMMAND_TIMEOUT_MS);
    signal?.addEventListener("abort", kill, { once: true });
    child.stdout.on("data", (chunk) => {
      if (Buffer.byteLength(output) + chunk.length > MAX_OUTPUT_BYTES) { overflow = true; kill(); }
      else output += chunk;
    });
    const done = () => { clearTimeout(timeout); signal?.removeEventListener("abort", kill); };
    child.on("error", (error) => { done(); reject(error); });
    child.on("close", (code) => {
      done();
      if (code === 0 && !overflow) resolve(output);
      else reject(new Error(`buzz ${args.filter((arg) => !arg.startsWith("-")).slice(0, 2).join(" ")} failed`));
    });
  });
}

const displayName = (output, pubkey) => {
  const profiles = JSON.parse(output);
  if (!Array.isArray(profiles)) throw new Error("unexpected users get output");
  const profile = profiles.find((entry) => pubkey === undefined || entry?.pubkey === pubkey);
  return typeof profile?.display_name === "string" ? profile.display_name : undefined;
};

/// One attempt. Resolves "published" or "unchanged"; rejects to retry.
export async function publishOnce(owner, { run = buzz, signal } = {}) {
  const name = gatewayName(displayName(await run(["--format", "compact", "users", "get", "--pubkey", owner], { signal }), owner));
  if (!name) throw new Error("the owner's Buzz profile has no name yet");
  // Without --pubkey, `users get` returns the caller's own profile: the gateway's.
  if (displayName(await run(["--format", "compact", "users", "get"], { signal })) === name) return "unchanged";
  await run(["users", "set-profile", "--name", name, "--about",
    `Paseo gateway for ${owner}. It posts the owner's Paseo sessions to Buzz.`], { signal });
  return "published";
}

/// Publish now, then retry with exponential backoff until it succeeds or the
/// retry window ends. close() cancels the timer and any running command.
export function createPublisher(owner, { run = buzz, retryMs = RETRY_MS, maxRetryMs = MAX_RETRY_MS,
  windowMs = RETRY_WINDOW_MS, now = Date.now } = {}) {
  const stop = new AbortController();
  const started = now();
  let timer;
  let attempts = 0;
  const finished = new Promise((resolve) => {
    const attempt = async () => {
      try {
        const result = await publishOnce(owner, { run, signal: stop.signal });
        log(result === "published" ? "gateway profile name published" : "gateway profile name already current");
        resolve(result);
      } catch (error) {
        if (stop.signal.aborted) { resolve("stopped"); return; }
        attempts += 1;
        if (now() - started >= windowMs) {
          log(`gateway profile not published within the retry window: ${error.message}`);
          resolve("gave-up");
          return;
        }
        const delay = Math.min(maxRetryMs, retryMs * 2 ** (attempts - 1));
        log(`gateway profile not published (${error.message}); retrying in ${Math.round(delay / 1000)}s`);
        timer = setTimeout(attempt, delay);
      }
    };
    stop.signal.addEventListener("abort", () => { clearTimeout(timer); resolve("stopped"); }, { once: true });
    attempt();
  });
  return { finished, close() { stop.abort(); return finished; } };
}

export default function contribute() {
  const owner = process.env.BUZZ_PASEO_OWNER ?? "";
  if (!HEX_PUBKEY.test(owner)) { log("BUZZ_PASEO_OWNER is unset; gateway profile disabled"); return () => {}; }
  const publisher = createPublisher(owner);
  return () => publisher.close();
}
