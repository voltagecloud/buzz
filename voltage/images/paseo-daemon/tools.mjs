#!/usr/bin/env node
// The model gets structured Buzz operations, never the gateway or Git token.
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const require = createRequire("/usr/local/lib/node_modules/@getpaseo/server/package.json");
const { Server } = await import(pathToFileURL(require.resolve("@modelcontextprotocol/sdk/server/index.js")));
const { StdioServerTransport } = await import(pathToFileURL(require.resolve("@modelcontextprotocol/sdk/server/stdio.js")));
const { CallToolRequestSchema, ListToolsRequestSchema } = await import(pathToFileURL(require.resolve("@modelcontextprotocol/sdk/types.js")));

const reads = {
  channels: [["channels", "list"], []],
  messages: [["messages", "get"], ["--channel", "--limit"]],
  thread: [["messages", "thread"], ["--channel", "--event"]],
  search: [["messages", "search"], ["--query", "--channel", "--limit"]],
  user: [["users", "get"], ["--name", "--pubkey"]],
  workflows_list: [["workflows", "list"], ["--channel"]],
  workflows_get: [["workflows", "get"], ["--workflow"]],
  workflows_runs: [["workflows", "runs"], ["--workflow", "--limit"]],
};
const writes = {
  message: [["messages", "send"], ["--channel", "--mention", "--reply-to"]],
  workflow_create: [["workflows", "create"], ["--channel"]],
  workflow_update: [["workflows", "update"], ["--channel", "--workflow"]],
  workflow_delete: [["workflows", "delete"], ["--workflow"]],
  workflow_trigger: [["workflows", "trigger"], ["--workflow", "--inputs"]],
};
const readOperations = [...Object.keys(reads), "memory_list", "memory_get", "memory_hash", "code_clone", "code_fetch", "code_diff", "code_show"];
const writeOperations = [...Object.keys(writes), "memory_set", "memory_patch", "memory_remove"];
const environment = Object.fromEntries(["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL", "USER",
  "BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY", "BUZZ_PASEO_OWNER", "BUZZ_AUTH_TAG",
  "GITHUB_READ_TOKEN", "GITHUB_READ_TOKEN_REPOSITORIES"].filter((key) => process.env[key] !== undefined).map((key) => [key, process.env[key]]));
const secrets = [environment.BUZZ_PRIVATE_KEY, environment.GITHUB_READ_TOKEN].filter(Boolean);
const redact = (value) => secrets.reduce((text, secret) => text.split(secret).join("[redacted]"), value);
const owner = environment.BUZZ_PASEO_OWNER;
const slug = /^[a-zA-Z0-9][a-zA-Z0-9/_-]{0,127}$/;

function flagArguments(args, allowed) {
  for (let i = 0; i < args.length; i += 2) {
    if (!allowed.includes(args[i]) || args[i + 1] === undefined || args[i + 1].startsWith("-")) throw new Error("unsupported operation arguments");
  }
  return args;
}

function invocation(name, input) {
  if (!input || typeof input !== "object" || Array.isArray(input) || Object.keys(input).some((key) => !["operation", "arguments", "text"].includes(key))) throw new Error("unsupported tool input");
  const { operation, arguments: args = [], text } = input ?? {};
  const options = name === "buzz_read" ? readOperations : name === "buzz_write" ? writeOperations : [];
  if (!options.includes(operation) || !Array.isArray(args) || args.length > 24 || args.some((a) => typeof a !== "string" || a.length > 2048 || a.includes("\0"))) throw new Error("unsupported operation arguments");
  if (text !== undefined && (typeof text !== "string" || Buffer.byteLength(text) > 65536)) throw new Error("text exceeds bound");
  const needsText = ["message", "workflow_create", "workflow_update", "memory_set", "memory_patch"].includes(operation);
  if (needsText !== (text !== undefined)) throw new Error("provide inline text only for message, workflow definition, or memory writes");
  if (operation.startsWith("memory_")) {
    if (!/^[0-9a-f]{64}$/.test(owner ?? "")) throw new Error("relay memory needs a configured owner");
    const action = operation.slice(7);
    const count = action === "list" ? 0 : action === "patch" ? 2 : 1;
    if (args.length !== count || (count && !slug.test(args[0])) || (action === "patch" && !/^[0-9a-f]{64}$/.test(args[1]))) throw new Error("invalid memory slug or base hash");
    const verb = { list: "ls", get: "get", hash: "hash", set: "set", patch: "patch", remove: "rm" }[action];
    return ["/usr/local/bin/buzz", ["mem", verb, ...(count ? [args[0]] : []),
      ...(action === "set" ? ["-"] : []), ...(action === "patch" ? ["--base-hash", args[1]] : []),
      "--owner", owner, ...(action === "list" ? ["--json"] : [])], text];
  }
  if (operation.startsWith("code_")) {
    if (args.length < 1 || args.length > 3 || args.some((arg) => arg.startsWith("-"))) throw new Error("provide repository and optional revisions");
    return ["/usr/local/bin/paseo-code", [operation.slice(5), ...args, "--inherit-process-group"]];
  }
  const [command, flags] = (name === "buzz_read" ? reads : writes)[operation];
  const extra = operation === "message" ? ["--content", "-"] : needsText ? [`--yaml=${text}`] : [];
  return ["/usr/local/bin/buzz", [...command, ...flagArguments(args, flags), ...extra], operation === "message" ? text : undefined];
}

const children = new Set();
const killGroup = (child) => {
  if (!child.pid) return;
  try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw new Error("could not contain tool process group"); }
};
for (const name of ["SIGTERM", "SIGINT"]) process.once(name, () => {
  for (const child of children) killGroup(child);
  process.exit(0);
});
process.stdin.once("end", () => { for (const child of children) killGroup(child); });

function execute([command, args, input], signal) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { env: environment, detached: true, stdio: ["pipe", "pipe", "pipe"] });
    children.add(child);
    let output = "", error = "", bytes = 0, failure;
    const stop = (reason) => {
      failure ??= reason;
      try { killGroup(child); } catch (error) { failure = error; }
    };
    const abort = () => stop(new Error("tool operation cancelled"));
    const timer = setTimeout(() => stop(new Error("tool operation timed out")), 180000);
    signal?.addEventListener("abort", abort, { once: true });
    for (const [stream, key] of [[child.stdout, "output"], [child.stderr, "error"]]) {
      stream.setEncoding("utf8");
      stream.on("data", (text) => {
        bytes += Buffer.byteLength(text);
        if (bytes > 512 * 1024) return stop(new Error("tool output exceeds bound; narrow the request"));
        if (key === "output") output += text; else error += text;
      });
    }
    const cleanup = () => { children.delete(child); clearTimeout(timer); signal?.removeEventListener("abort", abort); };
    child.once("error", (error) => { cleanup(); reject(error); });
    child.once("close", (code) => {
      cleanup();
      if (failure || code !== 0) reject(failure ?? new Error(`tool exited ${code}: ${redact(error).slice(0, 4096)}`));
      else resolve(redact(output));
    });
    child.stdin.on("error", () => {});
    child.stdin.end(input ?? "");
    if (signal?.aborted) abort();
  });
}

const tools = [
  { name: "buzz_read", description: "Read Buzz channels, threads, workflows, owner-scoped memory, or code. Flags match the corresponding buzz command. Memory get/hash takes [slug]; code operations take [owner/repository, optional revisions].",
    annotations: { readOnlyHint: true }, operations: readOperations },
  { name: "buzz_write", description: "Requires approval under the selected Codex review mode. Send a message, modify a workflow, or write owner-scoped relay memory. Supply exact message/YAML/memory text inline. Memory patch arguments are [slug, base_hash]; other memory writes take [slug]. Text is literal; file input is unsupported.",
    annotations: { readOnlyHint: false }, operations: writeOperations },
].map(({ operations, ...tool }) => ({ ...tool, inputSchema: { type: "object", additionalProperties: false, required: ["operation"], properties: {
  operation: { type: "string", enum: operations }, arguments: { type: "array", maxItems: 24, items: { type: "string", maxLength: 2048 } }, text: { type: "string", maxLength: 65536 },
} } }));
const server = new Server({ name: "buzz-personal-assistant", version: "1.0.0" }, { capabilities: { tools: {} } });
server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools }));
server.setRequestHandler(CallToolRequestSchema, async (request, { signal }) => {
  try {
    if (children.size >= 4) throw new Error("too many concurrent tool operations; retry after a current operation finishes");
    const text = await execute(invocation(request.params.name, request.params.arguments), signal);
    return { content: [{ type: "text", text: text || "Operation completed." }] };
  } catch (error) {
    return { isError: true, content: [{ type: "text", text: redact(String(error.message)).slice(0, 4096) }] };
  }
});
await server.connect(new StdioServerTransport());
