// Real Git smart-HTTP fixture. Only the network origin is rewritten locally;
// the production helper still constructs clone/fetch/show/diff operations.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { spawn, spawnSync } from "node:child_process";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const directory = await mkdtemp(path.join(os.tmpdir(), "paseo-code-git-"));
const helper = process.env.PASEO_CODE_HELPER ?? fileURLToPath(new URL("code.mjs", import.meta.url));
const env = { ...process.env, HOME: path.join(directory, "home"), GITHUB_READ_TOKEN: "github_pat_fixture_read_only", GIT_CONFIG_NOSYSTEM: "1" };
const git = (args, cwd) => {
  const result = spawnSync("git", args, { cwd, env, encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr); return result.stdout;
};
const backend = path.join(git(["--exec-path"]).trim(), "git-http-backend");
const server = http.createServer(async (request, response) => {
  const url = new URL(request.url, "http://fixture");
  const child = spawn(backend, [], { env: { ...env, GIT_PROJECT_ROOT: directory, GIT_HTTP_EXPORT_ALL: "1", PATH_INFO: url.pathname, QUERY_STRING: url.search.slice(1), REQUEST_METHOD: request.method, CONTENT_TYPE: request.headers["content-type"] ?? "", REMOTE_ADDR: "127.0.0.1", SERVER_PROTOCOL: "HTTP/1.1" } });
  let header = Buffer.alloc(0); let sent = false;
  child.stdout.on("data", (chunk) => {
    if (sent) { response.write(chunk); return; }
    header = Buffer.concat([header, chunk]);
    const at = header.indexOf("\r\n\r\n");
    if (at < 0) { if (header.length > 32768) child.kill(); return; }
    for (const line of header.subarray(0, at).toString().split("\r\n")) {
      const index = line.indexOf(":"); const key = line.slice(0, index); const value = line.slice(index + 1).trim();
      if (key.toLowerCase() === "status") response.statusCode = Number.parseInt(value, 10); else response.setHeader(key, value);
    }
    sent = true; response.write(header.subarray(at + 4)); header = Buffer.alloc(0);
  });
  child.stderr.resume(); child.on("error", () => { response.statusCode = 500; response.end(); });
  child.on("close", () => response.end()); request.pipe(child.stdin);
});
try {
  await mkdir(env.HOME); await mkdir(path.join(directory, "fixture"));
  const source = path.join(directory, "source"); await mkdir(source);
  git(["init", "-b", "main"], source);
  git(["config", "user.name", "Fixture"], source); git(["config", "user.email", "fixture@example.invalid"], source);
  await writeFile(path.join(source, "code.txt"), "initial code\n");
  git(["add", "."], source); git(["-c", "commit.gpgsign=false", "commit", "-m", "initial"], source);
  const origin = path.join(directory, "fixture", "repo.git");
  git(["clone", "--bare", source, origin]); git(["config", "uploadpack.allowFilter", "true"], origin); git(["config", "uploadpack.allowAnySHA1InWant", "true"], origin);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}/`;
  await writeFile(path.join(env.HOME, ".gitconfig"), `[url "${base}"]\n insteadOf = https://github.com/\n[protocol "http"]\n allow = always\n[core]\n hooksPath = /dev/null\n`);
  const run = (args) => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [helper, ...args], { env }); let output = ""; let errors = "";
    child.stdout.on("data", (chunk) => { output += chunk; }); child.stderr.on("data", (chunk) => { errors += chunk; }); child.on("error", reject);
    child.on("close", (code) => { try { assert.equal(code, 0, errors); resolve(output); } catch (error) { reject(error); } });
  });
  await run(["clone", "fixture/repo"]);
  const checkout = path.join(env.HOME, "code", "fixture", "repo");
  assert.equal(git(["config", "remote.origin.partialclonefilter"], checkout).trim(), "blob:none");
  assert.equal(await readFile(path.join(checkout, "code.txt"), "utf8"), "initial code\n");
  git(["switch", "-c", "review/branch"], source);
  await writeFile(path.join(source, "code.txt"), "sandbox pushed change\n");
  git(["add", "."], source); git(["-c", "commit.gpgsign=false", "commit", "-m", "sandbox change"], source);
  git(["push", origin, "review/branch"], source);
  await run(["fetch", "fixture/repo", "review/branch"]);
  assert.equal(await readFile(path.join(checkout, "code.txt"), "utf8"), "sandbox pushed change\n");
  const diff = await run(["diff", "fixture/repo", "origin/main", "HEAD"]);
  assert.match(diff, /\+sandbox pushed change/);
  assert.equal(git(["status", "--porcelain"], checkout), "");
  console.log("PASS: real Git partial clone, lazy file checkout, sandbox branch fetch, detached file view and diff");
} finally { await new Promise((resolve) => server.close(resolve)); await rm(directory, { recursive: true, force: true }); }
