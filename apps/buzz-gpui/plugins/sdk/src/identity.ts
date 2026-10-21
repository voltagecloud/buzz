// Public keys as buzz-app writes and shows them
// (`features/profiles/target.ts`, `shared/identity/public-key.ts`).

import { decode, npubEncode } from "nostr-tools/nip19";

/** Public identity locator, not a relay hint or an authority claim. */
export function profileTarget(pubkey: string, hint?: { agent: boolean }): string | undefined {
  return /^[a-f0-9]{64}$/i.test(pubkey)
    ? hint?.agent
      ? `buzz:agent-profile:${pubkey.toLowerCase()}`
      : `nostr:${npubEncode(pubkey.toLowerCase())}`
    : undefined;
}

/** Public message/composer input must never supply an app-local avatar hint. */
export function profileKey(target: string): string | undefined {
  if (!/^nostr:npub1[023456789acdefghjklmnpqrstuvwxyz]+$/.test(target)) return undefined;
  try {
    const decoded = decode(target.slice(6));
    return decoded.type === "npub" ? decoded.data : undefined;
  } catch {
    return undefined;
  }
}

/** The full npub of a hex public key, or undefined for anything else. */
export function canonicalPublicKey(pubkey: string): string | undefined {
  return /^[0-9a-f]{64}$/i.test(pubkey) ? npubEncode(pubkey.toLowerCase()) : undefined;
}

/** Display only: never use this abbreviation for copying, storage or routing. */
export function formatPublicKey(pubkey: string, suffixLength = 3): string | undefined {
  const npub = canonicalPublicKey(pubkey);
  if (!npub) return undefined;
  const length = Number.isInteger(suffixLength) ? Math.max(3, Math.min(suffixLength, npub.length - 5)) : 3;
  return `npub…${npub.slice(-length)}`;
}

/** Distinct public identities share the shortest unambiguous suffix, starting at three. */
export function publicKeyLabels(pubkeys: Iterable<string>): ReadonlyMap<string, string> {
  const encoded = new Map<string, string>();
  for (const pubkey of pubkeys) {
    const npub = canonicalPublicKey(pubkey);
    if (npub) encoded.set(pubkey.toLowerCase(), npub);
  }
  let length = 3;
  const values = [...encoded.values()];
  while (length < 58 && new Set(values.map((npub) => npub.slice(-length))).size < values.length) length++;
  return new Map([...encoded].map(([pubkey, npub]) => [pubkey, `npub…${npub.slice(-length)}`]));
}
