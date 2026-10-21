#!/usr/local/bin/node
// Fixed Codex enrollment boundary. Kubernetes operators and same-UID code are trusted.
'use strict';
const fs = require('node:fs');
const { performance } = require('node:perf_hooks');

const DIRECTORY = '/home/agent/.agent-setup';
const ACK = `${DIRECTORY}/complete.json`;
const CODEX_HOME = '/home/agent/.codex';
const CONFIG = 'cli_auth_credentials_store = "file"\n';
const SESSION_CONFIG = '{"cli_auth_credentials_store":"file"}';
const PATH = '/opt/buzz-heavy/bin:/opt/buzz-runtimes/node_modules/.bin:/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin';
const FIELDS = ['version', 'agent_pubkey', 'pod_uid', 'generation', 'image_digest', 'setup_nonce', 'deadline'];
const COMPLETION_FIELDS = ['version', 'agent_pubkey', 'pod_uid', 'generation', 'image_digest'];
const env = process.env;

function requireValue(condition) {
  if (!condition) throw new Error('refused');
}

function descriptor() {
  requireValue(process.getuid() === 10001 && process.getgid() === 10001);
  requireValue(env.HOME === '/home/agent' && process.cwd() === '/home/agent');
  requireValue(env.PATH === PATH);
  requireValue(env.BUZZ_SETUP_MODE === 'codex-device-v1');
  requireValue(env.BUZZ_ACP_AGENT_COMMAND === 'codex-acp');
  requireValue(!env.BUZZ_ACP_AGENT_ARGS);
  requireValue(env.BUZZ_ACP_MCP_COMMAND === 'buzz-dev-mcp');
  requireValue(env.BUZZ_ACP_PERMISSION_MODE === 'agent-full-access');
  requireValue(env.CODEX_HOME === CODEX_HOME);
  requireValue(env.CODEX_CONFIG === SESSION_CONFIG);
  requireValue(env.NO_BROWSER === '1');
  for (const key of Object.keys(env)) {
    if (/^(CODEX_|OPENAI_|ANTHROPIC_|CLAUDE_)/.test(key)) {
      requireValue(['CODEX_HOME', 'CODEX_CONFIG'].includes(key));
    }
    requireValue(!['MODEL_PROVIDER', 'DEFAULT_AUTH_REQUEST'].includes(key));
    if (key.startsWith('BUZZ_SETUP_')) {
      requireValue(['BUZZ_SETUP_MODE', 'BUZZ_SETUP_NONCE', 'BUZZ_SETUP_DEADLINE',
                    'BUZZ_SETUP_AGENT_PUBKEY', 'BUZZ_SETUP_POD_UID', 'BUZZ_SETUP_IMAGE_DIGEST'].includes(key));
    }
  }
  const value = {
    version: 1,
    agent_pubkey: env.BUZZ_SETUP_AGENT_PUBKEY,
    pod_uid: env.BUZZ_SETUP_POD_UID,
    generation: env.BUZZ_MANAGED_AGENT_START_NONCE,
    image_digest: env.BUZZ_SETUP_IMAGE_DIGEST,
    setup_nonce: env.BUZZ_SETUP_NONCE,
    deadline: Number(env.BUZZ_SETUP_DEADLINE),
  };
  requireValue(/^[0-9a-f]{64}$/.test(value.agent_pubkey || ''));
  requireValue(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value.pod_uid || ''));
  requireValue(/^(?:[0-9a-f]{8}|[0-9a-f]{32})$/.test(value.generation || ''));
  requireValue(/^sha256:[0-9a-f]{64}$/.test(value.image_digest || ''));
  requireValue(/^[0-9a-f]{32}$/.test(value.setup_nonce || ''));
  requireValue(/^[0-9]+$/.test(env.BUZZ_SETUP_DEADLINE || ''));
  requireValue(Number.isSafeInteger(value.deadline));
  checkDeadline(value);
  return value;
}

function checkDeadline(value) {
  const remaining = value.deadline - Date.now() / 1000;
  requireValue(remaining > 0 && remaining <= 900);
}

function privateDirectory(path) {
  const stat = fs.lstatSync(path);
  requireValue(stat.isDirectory() && !stat.isSymbolicLink());
  requireValue(stat.uid === 10001 && (stat.mode & 0o777) === 0o700);
}

function ensureDirectory(path) {
  try { fs.mkdirSync(path, { mode: 0o700 }); }
  catch (error) { if (error.code !== 'EEXIST') throw error; }
  privateDirectory(path);
}

function privateFile(path) {
  // Nonregular paths must reach fstat without waiting for a FIFO writer.
  const fd = fs.openSync(path, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
  try {
    const stat = fs.fstatSync(fd);
    requireValue(stat.isFile() && stat.uid === 10001 && stat.nlink === 1);
    requireValue((stat.mode & 0o777) === 0o600 && stat.size <= 4096);
    // Bound the read even if trusted same-UID code grows the file after fstat.
    const buffer = Buffer.alloc(4097);
    const length = fs.readSync(fd, buffer, 0, buffer.length, 0);
    requireValue(length <= 4096);
    return buffer.toString('utf8', 0, length);
  } finally { fs.closeSync(fd); }
}

function exclusiveFile(path, content) {
  const fd = fs.openSync(path, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_NOFOLLOW, 0o600);
  try { fs.writeFileSync(fd, content); }
  finally { fs.closeSync(fd); }
}

function absent(path) {
  try { fs.lstatSync(path); }
  catch (error) { if (error.code === 'ENOENT') return; throw error; }
  throw new Error('refused');
}

function validateAck(text, expected, fields = FIELDS) {
  requireValue(Buffer.byteLength(text) <= 4096);
  // Bindings have fixed scalar formats, so property counts also reject duplicate keys.
  requireValue((text.match(/"(?:[^"\\]|\\.)*"\s*:/g) || []).length === fields.length);
  const value = JSON.parse(text);
  requireValue(value && typeof value === 'object' && !Array.isArray(value));
  requireValue(Object.keys(value).length === fields.length);
  for (const key of fields) requireValue(Object.hasOwn(value, key) && value[key] === expected[key]);
  checkDeadline(expected);
}

async function complete(expected) {
  privateDirectory(DIRECTORY);
  validateAck(privateFile(`${DIRECTORY}/pending.json`), expected);
  absent(`${DIRECTORY}/consumed`);
  let input = '';
  for await (const chunk of process.stdin) {
    input += chunk.toString('utf8');
    requireValue(Buffer.byteLength(input) <= 4096);
  }
  validateAck(input, expected, COMPLETION_FIELDS);
  privateDirectory(CODEX_HOME);
  requireValue(privateFile(`${CODEX_HOME}/config.toml`) === CONFIG);
  absent(ACK);
  // The retained exclusive claim prevents a second helper invocation or replay.
  exclusiveFile(`${DIRECTORY}/ack-claimed`, 'claimed\n');
  exclusiveFile(`${DIRECTORY}/ack-staged`, JSON.stringify(expected));
  fs.renameSync(`${DIRECTORY}/ack-staged`, ACK);
  console.log('agent setup completion accepted');
}

async function gate(expected) {
  requireValue(typeof process.execve === 'function');
  ensureDirectory(DIRECTORY);
  exclusiveFile(`${DIRECTORY}/gate-claimed`, 'claimed\n');
  absent(ACK);
  absent(`${DIRECTORY}/consumed`);
  ensureDirectory(CODEX_HOME);
  absent(`${CODEX_HOME}/auth.json`);
  exclusiveFile(`${CODEX_HOME}/config.toml`, CONFIG);
  exclusiveFile(`${DIRECTORY}/pending.json`, JSON.stringify(expected));
  console.log('agent setup pending');
  const started = performance.now();
  const remainingMs = (expected.deadline - Date.now() / 1000) * 1000;
  while (true) {
    checkDeadline(expected);
    requireValue(performance.now() - started < remainingMs);
    privateDirectory(DIRECTORY);
    let exists = false;
    try { fs.lstatSync(ACK); exists = true; }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    if (exists) {
      validateAck(privateFile(ACK), expected);
      privateDirectory(CODEX_HOME);
      requireValue(privateFile(`${CODEX_HOME}/config.toml`) === CONFIG);
      exclusiveFile(`${DIRECTORY}/consumed`, 'consumed\n');
      fs.renameSync(ACK, `${DIRECTORY}/accepted`);
      fs.unlinkSync(`${DIRECTORY}/accepted`);
      fs.unlinkSync(`${DIRECTORY}/pending.json`);
      env.CODEX_HOME = CODEX_HOME;
      env.CODEX_CONFIG = SESSION_CONFIG;
      env.NO_BROWSER = '1';
      console.log('agent setup complete; starting harness');
      process.execve('/usr/local/bin/buzz-agent-cache', ['/usr/local/bin/buzz-agent-cache', '/usr/local/bin/buzz-acp'], env);
      throw new Error('refused');
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
}

process.on('SIGTERM', () => process.exit(143));
process.on('SIGINT', () => process.exit(130));
(async () => {
  const helper = process.argv[1] === '/usr/local/bin/buzz-setup-complete';
  requireValue(helper ? process.argv.length === 2 : process.argv.length === 3 && ['gate', 'complete'].includes(process.argv[2]));
  const expected = descriptor();
  if (!helper && process.argv[2] === 'gate') await gate(expected);
  else await complete(expected);
})().catch(() => {
  console.error('agent setup refused');
  process.exitCode = 1;
});
