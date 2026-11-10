import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, chmod, readFile, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const directory = await mkdtemp(path.join(os.tmpdir(), "paseo-code-test-"));
const helper = process.env.PASEO_CODE_HELPER ?? fileURLToPath(new URL("code.mjs", import.meta.url));
try {
  await mkdir(path.join(directory, "bin"));
  await writeFile(path.join(directory, "bin", "git"), '#!/usr/bin/env node\nrequire("node:fs").appendFileSync(process.env.GIT_ARGS_TEST, JSON.stringify(process.argv.slice(2))+"\\n");\n');
  await chmod(path.join(directory, "bin", "git"), 0o755);
  const token = "github_pat_synthetic_selected_read_only";
  const env = { ...process.env, HOME: directory, PATH: `${directory}/bin:${process.env.PATH}`, GITHUB_READ_TOKEN: token,
    GITHUB_READ_TOKEN_REPOSITORIES: '["VoltageCloud/Backend","fixture/private.git"]', GIT_ARGS_TEST: path.join(directory, "args") };
  const run = (args, input) => spawnSync(process.execPath, [helper, ...args], { env, input, encoding: "utf8", timeout: 2000 });
  let result = run(["clone", "voltagecloud/buzz"]);
  assert.equal(result.status, 0, result.stderr);
  const clone = JSON.parse((await readFile(env.GIT_ARGS_TEST, "utf8")).trim());
  assert.deepEqual(clone.slice(0, 4), ["clone", "--filter=blob:none", "--no-tags", "--"]);
  assert.ok(!clone.join(" ").includes(token));
  await mkdir(path.join(directory, "code", "voltagecloud", "buzz"));
  await writeFile(env.GIT_ARGS_TEST, "");
  result = run(["fetch", "voltagecloud/buzz", "review/branch"]); assert.equal(result.status, 0, result.stderr);
  const fetched = (await readFile(env.GIT_ARGS_TEST, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(fetched[1].slice(2), ["switch", "--detach", "refs/remotes/origin/review/branch"]);
  assert.deepEqual(fetched[0].slice(2), ["fetch", "--filter=blob:none", "--no-tags", "--", "origin", "+refs/heads/review/branch:refs/remotes/origin/review/branch"]);
  for (const args of [["push", "voltagecloud/buzz"], ["clone", "../escape"], ["fetch", "voltagecloud/buzz", "--upload-pack=bad"], ["diff", "voltagecloud/buzz", "main..bad"]]) assert.equal(run(args).status, 1);
  assert.equal(run(["scope"]).stdout, '["voltagecloud/backend","fixture/private.git"]');
  result = run(["credential", "get"], "protocol=https\nhost=github.com\npath=VOLTAGECLOUD/backend.git\n\n");
  assert.equal(result.stdout, `username=x-access-token\npassword=${token}\n\n`);
  for (const scoped of ["voltagecloud/backend", "VOLTAGECLOUD/BACKEND.GIT", "fixture/private.git.git"]) assert.match(run(["credential", "get"], `protocol=https\nhost=github.com\npath=${scoped}\n\n`).stdout, /password=/);
  for (const [protocol, host, requested] of [["https", "evil.test", "voltagecloud/backend.git"], ["http", "github.com", "voltagecloud/backend.git"],
    ["https", "github.com:443", "voltagecloud/backend.git"], ["https", "github.com", "lightningnetwork/lnd.git"], ["https", "github.com", "voltagecloud/backend-other.git"],
    ["https", "github.com", "voltagecloud/backend.git/extra"], ["https", "github.com", "voltagecloud/../backend.git"], ["https", "github.com", "voltagecloud%2fbackend.git"],
    ["https", "github.com", "voltagecloud/backend.git "], ["https", "github.com", "/voltagecloud/backend.git"], ["https", "github.com", ""]]) {
    assert.equal(run(["credential", "get"], `protocol=${protocol}\nhost=${host}\npath=${requested}\n\n`).stdout, "");
  }
  assert.equal(run(["credential", "get"], "protocol=https\nhost=evil.test\nhost=github.com\npath=voltagecloud/backend.git\n\n").stdout, "");
  for (const invalid of ['{}', '["voltagecloud/backend","VoltageCloud/Backend"]', '["../backend"]', '["fixture/private..repo"]',
    '["https://github.com/voltagecloud/backend"]', JSON.stringify(Array.from({ length: 65 }, (_, index) => `fixture/private${index}`)), " ".repeat(16385) + "[]"]) {
    env.GITHUB_READ_TOKEN_REPOSITORIES = invalid;
    assert.equal(run(["scope"]).status, 1);
    assert.equal(run(["credential", "get"], "protocol=https\nhost=github.com\npath=voltagecloud/backend.git\n\n").stdout, "");
  }
  delete env.GITHUB_READ_TOKEN_REPOSITORIES;
  assert.equal(run(["credential", "get"], "protocol=https\nhost=github.com\npath=voltagecloud/backend.git\n\n").stdout, "");
  delete env.GITHUB_READ_TOKEN;
  env.GITHUB_READ_TOKEN_REPOSITORIES = '["voltagecloud/backend"]';
  assert.equal(run(["credential", "get"], "protocol=https\nhost=github.com\npath=voltagecloud/backend.git\n\n").stdout, "");
  assert.equal(run(["clone", "fixture/public"]).status, 0, "anonymous public clone must not require a token");
  assert.equal(run(["fetch", "voltagecloud/buzz"]).status, 0, "anonymous public fetch must not require a token");
  const configuredPath = env.PATH; env.PATH = "/nonexistent";
  assert.equal(run(["clone", "fixture/missing-git"]).status, 1, "spawn failure must clear its deadline and exit promptly");
  env.PATH = configuredPath;
  console.log("PASS: partial clone/fetch, anonymous public reads, bounded configured credential paths, canonicalization, and no token in argv");
} finally { await rm(directory, { recursive: true, force: true }); }
