// Agents' reported token and cost counters, from your saved turn metrics
// (buzz-app `features/agents/usage.ts`).

/** One saved record, as `archive/read` gives it with its `frame` as text. */
export type ArchivePage = Readonly<{
  records: readonly Readonly<{
    id: string;
    agent: string;
    createdAt: number;
    receivedAt: number;
    plaintext: string;
  }>[];
}>;

export type Counters = Readonly<{
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
  costUsd?: number;
}>;
export type UsageRecord = Readonly<{
  id: string;
  agent: string;
  sessionId: string | null;
  turnId: string | null;
  turnSeq: number | null;
  timestamp: number;
  harness: string;
  model: string | null;
  stopReason: string | null;
  pricing: string | null;
  turn: Counters | null;
  cumulative: Counters | null;
  deltaReliable: boolean;
  conflict: boolean;
}>;
export type UsageSession = Readonly<{
  key: string;
  agent: string;
  sessionId: string | null;
  latest: UsageRecord | null;
  turns: readonly UsageRecord[];
}>;
const object = (value: unknown): Record<string, unknown> | null =>
  value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
const label = (value: unknown): value is string =>
  typeof value === "string" && value.length > 0 && value.length <= 256;
function counters(value: unknown): Counters | null {
  const raw = object(value);
  if (!raw) return null;
  const result: Record<string, number> = {};
  for (const key of [
    "inputTokens",
    "outputTokens",
    "totalTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "costUsd",
  ]) {
    const number = raw[key];
    if (number === undefined || number === null) continue;
    if (
      typeof number !== "number" ||
      !Number.isFinite(number) ||
      number < 0 ||
      (key !== "costUsd" && !Number.isSafeInteger(number))
    )
      return null;
    result[key] = number;
  }
  return result;
}
/** Host validates envelope/signature/recipient. This parser validates only decoded metric semantics. */
export function decodeUsage(
  record: ArchivePage["records"][number],
  channelId: string,
): UsageRecord | null {
  let raw: Record<string, unknown> | null;
  try {
    raw = object(JSON.parse(record.plaintext));
  } catch {
    return null;
  }
  if (
    !raw ||
    raw.channelId !== channelId ||
    !label(raw.harness) ||
    typeof raw.timestamp !== "string"
  )
    return null;
  const timestamp = Date.parse(raw.timestamp);
  if (
    !Number.isFinite(timestamp) ||
    (raw.turnSeq != null &&
      (!Number.isSafeInteger(raw.turnSeq) || (raw.turnSeq as number) < 0)) ||
    (raw.turnId != null && !label(raw.turnId)) ||
    (raw.model != null && !label(raw.model)) ||
    (raw.deltaReliable != null && typeof raw.deltaReliable !== "boolean")
  )
    return null;
  const turn = raw.turn == null ? null : counters(raw.turn);
  const cumulative = raw.cumulative == null ? null : counters(raw.cumulative);
  if ((raw.turn != null && !turn) || (raw.cumulative != null && !cumulative))
    return null;
  const usableCumulative =
    cumulative && label(raw.sessionId) && Number.isSafeInteger(raw.turnSeq)
      ? cumulative
      : null;
  const pricing = object(raw.pricingIdentity);
  if (
    raw.pricingIdentity !== undefined &&
    (!pricing || !label(pricing.authority) || !label(pricing.model))
  )
    return null;
  if (pricing && pricing.cacheClass !== undefined && !label(pricing.cacheClass))
    return null;
  const stopReason =
    typeof raw.stopReason === "string" &&
    ["end_turn", "max_tokens", "cancelled", "error", "unknown"].includes(
      raw.stopReason,
    )
      ? raw.stopReason
      : raw.stopReason == null
        ? null
        : "unknown";
  return {
    id: record.id,
    agent: record.agent,
    sessionId: label(raw.sessionId) ? raw.sessionId : null,
    turnId: label(raw.turnId) ? raw.turnId : null,
    turnSeq: typeof raw.turnSeq === "number" ? raw.turnSeq : null,
    timestamp,
    harness: raw.harness,
    model: label(raw.model) ? raw.model : null,
    stopReason,
    pricing: pricing
      ? `${pricing.authority} · ${pricing.model}${label(pricing.cacheClass) ? ` · ${pricing.cacheClass}` : ""}`
      : null,
    turn: raw.deltaReliable === false ? null : turn,
    cumulative: usableCumulative,
    deltaReliable: raw.deltaReliable !== false,
    conflict: false,
  };
}
export function projectUsage(
  records: readonly ArchivePage["records"][number][],
  channelId: string,
): UsageSession[] {
  return projectUsageWithUnreadable(records, channelId).groups;
}

export function projectUsageWithUnreadable(
  records: readonly ArchivePage["records"][number][],
  channelId: string,
): { groups: UsageSession[]; unreadable: number } {
  let unreadable = 0;
  const groups = new Map<string, UsageRecord[]>();
  const seen = new Set<string>();
  for (const frame of records) {
    if (seen.has(frame.id)) continue;
    seen.add(frame.id);
    const item = decodeUsage(frame, channelId);
    if (!item) {
      try {
        if (object(JSON.parse(frame.plaintext))?.channelId === channelId)
          unreadable++;
      } catch {
        // Without a decoded channel ID this frame cannot be attributed here.
      }
      continue;
    }
    // Missing session identities must never collapse into a fictitious session.
    const key = JSON.stringify([
      item.agent,
      item.sessionId === null ? "unidentified" : "session",
      item.sessionId ?? frame.id,
    ]);
    groups.set(key, [...(groups.get(key) ?? []), item]);
  }
  return {
    groups: [...groups]
      .map(([key, items]) => {
        const ordered = items.sort(
          (a, b) =>
            (b.turnSeq ?? -1) - (a.turnSeq ?? -1) ||
            b.timestamp - a.timestamp ||
            b.id.localeCompare(a.id),
        );
        const conflicts = new Set<string>();
        const logical = new Map<string, string>();
        for (const item of ordered) {
          for (const identity of [
            ...(item.turnId !== null ? [`turn:${item.turnId}`] : []),
            ...(item.turnSeq !== null ? [`seq:${item.turnSeq}`] : []),
          ]) {
            const previous = logical.get(identity);
            if (previous) {
              conflicts.add(previous);
              conflicts.add(item.id);
            } else logical.set(identity, item.id);
          }
        }
        const turns = ordered.map((item) =>
          conflicts.has(item.id)
            ? { ...item, turn: null, conflict: true }
            : item,
        );
        const first = ordered[0];
        if (!first) throw new Error("Empty usage group");
        const newest = ordered.find((item) => item.cumulative !== null);
        const ambiguousLatest =
          newest &&
          ordered.some(
            (item) =>
              conflicts.has(item.id) &&
              item.turnSeq !== null &&
              newest.turnSeq !== null &&
              item.turnSeq >= newest.turnSeq,
          );
        return {
          key,
          agent: first.agent,
          sessionId: first.sessionId,
          latest: first.sessionId && newest && !ambiguousLatest ? newest : null,
          turns,
        };
      })
      .sort(
        (a, b) =>
          (b.turns[0]?.timestamp ?? 0) - (a.turns[0]?.timestamp ?? 0) ||
          a.key.localeCompare(b.key),
      ),
    unreadable,
  };
}

/** Sum each session's latest trusted snapshot once; unknown fields stay unknown. */
export function aggregateSessionUsage(groups: readonly UsageSession[]): {
  counters: Counters;
  complete: number;
} {
  const keys = [
    "totalTokens",
    "inputTokens",
    "outputTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "costUsd",
  ] as const;
  const snapshots = groups.map((group) => group.latest?.cumulative ?? null);
  const counters: Partial<Record<(typeof keys)[number], number>> = {};
  for (const key of keys) {
    // Never silently turn an absent counter into zero or mix incompatible coverage.
    if (
      snapshots.length &&
      snapshots.every((item) => item?.[key] !== undefined)
    ) {
      const sum = snapshots.reduce(
        (value, item) => value + (item?.[key] ?? 0),
        0,
      );
      if (
        Number.isFinite(sum) &&
        (key === "costUsd" || Number.isSafeInteger(sum))
      )
        counters[key] = sum;
    }
  }
  return {
    counters,
    complete: snapshots.filter((item) => item !== null).length,
  };
}
