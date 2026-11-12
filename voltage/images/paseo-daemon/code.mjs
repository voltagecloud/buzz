#!/usr/bin/env node
// GitHub token permissions, not this convenience command, enforce read access.
import { mkdir, lstat, realpath } from "node:fs/promises";
import { spawn } from "node:child_process";
import path from "node:path";

const fail = (message) => { console.error(`paseo-code: ${message}`); process.exitCode = 1; };
const [action, repository, ...args] = process.argv.slice(2);
const canonicalRepository = (value) => {
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,99}\/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$/.test(value) || value.includes("..")) throw new Error("use owner/repository without traversal");
  return value.toLowerCase();
};
const tokenRepositories = () => {
  const raw = process.env.GITHUB_READ_TOKEN_REPOSITORIES ?? "[]";
  try {
    if (Buffer.byteLength(raw) > 16384) throw new Error();
    const entries = JSON.parse(raw);
    if (!Array.isArray(entries) || entries.length > 64) throw new Error();
    const normalized = entries.map(canonicalRepository);
    if (new Set(normalized).size !== entries.length) throw new Error();
    return normalized;
  } catch { throw new Error("GITHUB_READ_TOKEN_REPOSITORIES must contain at most 64 unique owner/repository names in a JSON array of at most 16384 bytes"); }
};
try {
  const scopedRepositories = tokenRepositories();
  if (action === "scope") {
    if (repository || args.length) throw new Error("scope accepts no arguments");
    process.stdout.write(JSON.stringify(scopedRepositories));
  } else if (action === "credential") {
    // Git invokes this through credential.helper. Never put the token in a URL,
    // process argument, repository config, assistant prompt, or normal output.
    let input = "";
    for await (const chunk of process.stdin) {
      input += chunk;
      if (Buffer.byteLength(input) > 4096) throw new Error("credential request exceeds bound");
    }
    const fields = Object.create(null);
    for (const line of input.split("\n")) {
      if (line === "") continue;
      const at = line.indexOf("=");
      if (at < 1 || Object.hasOwn(fields, line.slice(0, at))) throw new Error("invalid credential request");
      fields[line.slice(0, at)] = line.slice(at + 1);
    }
    let requested;
    try { requested = canonicalRepository(fields.path?.replace(/\.git$/i, "")); } catch { /* Unrecognized paths never receive credentials. */ }
    if (repository === "get" && fields.protocol === "https" && fields.host === "github.com" && scopedRepositories.includes(requested) && process.env.GITHUB_READ_TOKEN) {
      process.stdout.write(`username=x-access-token\npassword=${process.env.GITHUB_READ_TOKEN}\n\n`);
    }
  } else {
    const normalized = canonicalRepository(repository);
    if (!["clone", "fetch", "diff", "show"].includes(action)) throw new Error("use clone, fetch, diff, or show");
    if (args.length > 2 || args.some((ref) => !/^[A-Za-z0-9][A-Za-z0-9_./-]{0,200}$/.test(ref) || /\.\.|\/\/|\/$|\.$|\.lock(?:\/|$)/.test(ref))) throw new Error("invalid branch or revision");
    const root = path.join(process.env.HOME, "code");
    const directory = path.join(root, normalized);
    await mkdir(path.dirname(directory), { recursive: true, mode: 0o700 });
    if (await realpath(path.dirname(directory)) !== path.dirname(directory)) throw new Error("checkout parent must not be a symlink");
    if (action !== "clone") {
      const info = await lstat(directory);
      if (!info.isDirectory() || info.isSymbolicLink()) throw new Error("checkout must be a directory");
    }
    const command = action === "clone"
      ? ["clone", "--filter=blob:none", "--no-tags", ...(args[0] ? ["--branch", args[0]] : []), "--", `https://github.com/${normalized}.git`, directory]
      : action === "fetch"
        ? ["-C", directory, "fetch", "--filter=blob:none", "--no-tags", "--", "origin", ...(args[0] ? [`+refs/heads/${args[0]}:refs/remotes/origin/${args[0]}`] : [])]
        : action === "diff"
          ? ["-C", directory, "diff", "--no-ext-diff", "--no-textconv", args[0] ?? "HEAD", args[1] ?? "origin/HEAD", "--"]
          : ["-C", directory, "show", "--no-ext-diff", "--no-textconv", args[0] ?? "HEAD", "--"];
    const commands = [command, ...(action === "fetch" && args[0]
      ? [["-C", directory, "switch", "--detach", `refs/remotes/origin/${args[0]}`]] : [])];
    const deadline = Date.now() + 180000;
    for (const command of commands) {
      const child = spawn("git", command, { detached: true, stdio: "inherit", env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GIT_OPTIONAL_LOCKS: "0" } });
      const kill = () => { try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") fail("cannot stop Git process group"); } };
      const timer = setTimeout(kill, Math.max(1, deadline - Date.now()));
      process.once("SIGTERM", kill); process.once("SIGINT", kill);
      let code;
      try { code = await new Promise((resolve, reject) => { child.once("error", reject); child.once("close", resolve); }); }
      finally {
        clearTimeout(timer);
        process.removeListener("SIGTERM", kill); process.removeListener("SIGINT", kill);
      }
      if (code !== 0 || Date.now() >= deadline) { process.exitCode = 1; break; }
    }
  }
} catch (error) { fail(error.message); }
