// The Buzz remote-agent providers in the daemon's config.json, built from
// the manager's agent list. Shared by the entrypoint at startup and by
// paseo-agents-watch when the list changes, so both validate identically.

export const HEX_PUBKEY = /^[0-9a-f]{64}$/;
export const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/** At most this many remote providers. */
export const MAX_AGENTS = 64;

/** One Buzz provider entry for `agent`, acting for `owner`. */
function provider({ label, agent: pubkey, channel }, owner) {
  // buzz acp-bridge gets BUZZ_RELAY_URL and BUZZ_PRIVATE_KEY from the
  // inherited environment, so config.json holds no secrets. The env overlay
  // blanks the secrets that the bridge does not use. The bridge gets no Paseo
  // tools, no MCP servers, no terminals, and no file access from the daemon.
  return {
    extends: "acp",
    label,
    command: ["buzz", "acp-bridge", "--agent", pubkey, "--channel", channel, "--owner", owner],
    env: { PASEO_PASSWORD: "", CLAUDE_CODE_OAUTH_TOKEN: "", CODEX_ACCESS_TOKEN: "", GITHUB_READ_TOKEN: "", github_read_token: "" },
    options: {
      supportsMcpServers: false,
      clientCapabilities: { terminal: false, fs: { readTextFile: false, writeTextFile: false } },
    },
    paseoTools: { enabled: false },
    // One model, preselected: the bridge offers no ACP model choice, so the
    // picker should not ask for one.
    models: [{ id: "buzz", label, isDefault: true }],
  };
}

/**
 * Validate `agents` and return `{id: provider}`. `taken` holds provider ids
 * that are reserved. Throws an Error naming the first invalid field.
 */
export function buzzProviders(agents, owner, taken) {
  if (!Array.isArray(agents) || agents.length > MAX_AGENTS) {
    throw new Error(`BUZZ_PASEO_AGENTS must be a JSON array of at most ${MAX_AGENTS} providers`);
  }
  if (owner && !HEX_PUBKEY.test(owner)) throw new Error("BUZZ_PASEO_OWNER must be a 64-character hex pubkey");
  if (agents.length && !owner) throw new Error("BUZZ_PASEO_OWNER is required with remote providers");
  const providers = {};
  for (const [index, agent] of agents.entries()) {
    const where = `BUZZ_PASEO_AGENTS[${index}]`;
    const { id, label, agent: pubkey, channel } = agent ?? {};
    if (typeof id !== "string" || !/^[a-z][a-z0-9-]*$/.test(id)) throw new Error(`${where}.id must match ^[a-z][a-z0-9-]*$`);
    if (taken.includes(id) || Object.hasOwn(providers, id)) throw new Error(`${where}.id "${id}" is reserved or duplicated`);
    if (typeof label !== "string" || label.trim() === "") throw new Error(`${where}.label must be a non-empty string`);
    if (typeof pubkey !== "string" || !HEX_PUBKEY.test(pubkey)) throw new Error(`${where}.agent must be a 64-character hex pubkey`);
    if (typeof channel !== "string" || !UUID.test(channel)) throw new Error(`${where}.channel must be a UUID`);
    providers[id] = provider(agent, owner);
  }
  return providers;
}

/** Whether `entry` is a Buzz remote-agent provider this module wrote. */
export function isBuzzProvider(entry) {
  return entry?.extends === "acp" && entry?.command?.[0] === "buzz" && entry?.command?.[1] === "acp-bridge";
}

/**
 * `config` with its Buzz providers replaced by those for `agents`; every
 * other provider is kept. Throws, leaving `config` unchanged, when `agents`
 * is invalid.
 */
export function withAgents(config, agents, owner) {
  const current = config?.agents?.providers ?? {};
  const kept = Object.fromEntries(Object.entries(current).filter(([, entry]) => !isBuzzProvider(entry)));
  const added = buzzProviders(agents, owner, Object.keys(kept));
  return { ...config, agents: { ...config.agents, providers: { ...kept, ...added } } };
}
