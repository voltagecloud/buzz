import { describe, expect, it } from "vitest";
import {
  aggregateSessionUsage,
  decodeUsage,
  projectUsage,
  projectUsageWithUnreadable,
  type ArchivePage,
} from "./usage.ts";
const agent = (n: number) => n.toString(16).padStart(64, "0");
const frame = (n: number, payload: Record<string, unknown>, owner = agent(1)) =>
  ({
    id: agent(n + 10),
    agent: owner,
    createdAt: 100,
    receivedAt: n,
    plaintext: JSON.stringify({
      harness: "goose",
      timestamp: new Date(1000 * (100 + n)).toISOString(),
      channelId: "selected",
      ...payload,
    }),
  }) satisfies ArchivePage["records"][number];

describe("channel usage projection", () => {
  it("counts invalid matching rows once while excluding unrelated and duplicate records", () => {
    const invalid = frame(1, { timestamp: "invalid" });
    const unrelated = frame(2, { channelId: "other", timestamp: "invalid" });
    const valid = frame(3, { sessionId: "s", turnSeq: 1 });
    expect(
      projectUsageWithUnreadable(
        [invalid, invalid, unrelated, valid],
        "selected",
      ),
    ).toMatchObject({
      unreadable: 1,
      groups: [{ sessionId: "s", turns: [{ id: valid.id }] }],
    });
  });
  it("keeps three agent identities, two sessions each and 32 turns each separate", () => {
    const rows = Array.from({ length: 3 }, (_, a) =>
      Array.from({ length: 32 }, (_, t) =>
        frame(
          a * 100 + t,
          {
            sessionId: `session-${Math.floor(t / 16)}`,
            turnId: `turn-${t}`,
            turnSeq: t % 16,
            turn: { inputTokens: t, cacheReadTokens: 0 },
            cumulative: { inputTokens: t * 2, totalTokens: null },
          },
          agent(a + 1),
        ),
      ),
    )
      .flat()
      .reverse();
    const groups = projectUsage(rows, "selected");
    expect(groups).toHaveLength(6);
    expect(groups.every((group) => group.turns.length === 16)).toBe(true);
    expect(groups.every((group) => group.latest?.turnSeq === 15)).toBe(true);
    expect(groups[0]?.latest?.cumulative?.totalTokens).toBeUndefined();
    expect(groups[0]?.turns.at(-1)?.turn?.cacheReadTokens).toBe(0);
  });
  it("uses only the latest valid cumulative snapshot without backfilling unknown fields", () => {
    const rows = [
      frame(1, {
        sessionId: "s",
        turnSeq: 1,
        cumulative: { totalTokens: 30, costUsd: 1 },
      }),
      frame(2, {
        sessionId: "s",
        turnSeq: 2,
        cumulative: {
          inputTokens: 0,
          outputTokens: 4,
          totalTokens: null,
          cacheReadTokens: 0,
        },
      }),
      frame(3, { sessionId: "s", turnSeq: 3, turn: { inputTokens: 3 } }),
    ];
    expect(projectUsage(rows, "selected")[0]?.latest?.cumulative).toEqual({
      inputTokens: 0,
      outputTokens: 4,
      cacheReadTokens: 0,
    });
  });
  it("does not conflate unidentified sessions, duplicate deliveries, or conflicting turns", () => {
    const initial = frame(1, {
      sessionId: "s",
      turnId: "id:initial",
      turnSeq: 0,
    });
    const first = frame(2, {
      sessionId: "s",
      turnId: "id",
      turnSeq: 1,
      turn: { inputTokens: 3 },
    });
    const conflict = frame(3, {
      sessionId: "s",
      turnId: "id",
      turnSeq: 1,
      turn: { inputTokens: 8 },
    });
    const groups = projectUsage(
      [
        initial,
        first,
        conflict,
        first,
        frame(4, { sessionId: null }),
        frame(5, { sessionId: null }),
      ],
      "selected",
    );
    expect(groups).toHaveLength(3);
    const known = groups.find((group) => group.sessionId === "s");
    expect(known?.turns).toHaveLength(3);
    expect(known?.turns.filter((item) => item.conflict)).toHaveLength(2);
    expect(
      known?.turns.find((item) => item.turnId === "id:initial")?.conflict,
    ).toBe(false);
    const reusedSequence = projectUsage(
      [
        frame(10, {
          sessionId: "x",
          turnId: "a",
          turnSeq: 1,
          turn: { inputTokens: 2 },
        }),
        frame(11, {
          sessionId: "x",
          turnId: "b",
          turnSeq: 1,
          turn: { inputTokens: 3 },
        }),
      ],
      "selected",
    );
    expect(
      reusedSequence[0]?.turns.every(
        (item) => item.conflict && item.turn === null,
      ),
    ).toBe(true);
  });
  it("does not choose an arbitrary cumulative winner when sequence evidence conflicts", () => {
    const rows = [
      frame(30, {
        sessionId: "s",
        turnId: "a",
        turnSeq: 1,
        cumulative: { totalTokens: 100 },
      }),
      frame(31, {
        sessionId: "s",
        turnId: "b",
        turnSeq: 2,
        cumulative: { totalTokens: 200 },
      }),
      frame(32, {
        sessionId: "s",
        turnId: "c",
        turnSeq: 2,
        cumulative: { totalTokens: 999 },
      }),
    ];
    const group = projectUsage(rows, "selected")[0];
    expect(group?.latest).toBeNull();
    expect(group?.turns.filter((item) => item.conflict)).toHaveLength(2);
  });
  it("rejects a nullable pricing identity and separates unidentified IDs from reported session IDs", () => {
    expect(
      decodeUsage(frame(20, { pricingIdentity: null }), "selected"),
    ).toBeNull();
    expect(
      decodeUsage(frame(23, { stopReason: "future_reason" }), "selected")
        ?.stopReason,
    ).toBe("unknown");
    const unidentified = frame(21, { sessionId: null });
    const identified = frame(22, { sessionId: unidentified.id, turnSeq: 0 });
    expect(projectUsage([unidentified, identified], "selected")).toHaveLength(
      2,
    );
  });
  it("does not present unreliable turn usage or malformed and unrelated metrics", () => {
    const bad = frame(6, { turn: { inputTokens: -1 } });
    const other = frame(7, { channelId: "other" });
    const channelLess = frame(8, { channelId: null });
    const unreliable = frame(9, {
      deltaReliable: false,
      turn: { inputTokens: 3 },
      sessionId: "s",
      turnSeq: 1,
      cumulative: { inputTokens: 3 },
    });
    expect(decodeUsage(bad, "selected")).toBeNull();
    expect(
      projectUsage([bad, other, channelLess, unreliable], "selected")[0]
        ?.turns[0]?.turn,
    ).toBeNull();
  });
});

describe("cross-session totals", () => {
  it("uses each latest cumulative snapshot once, not the sum of every turn", () => {
    const groups = projectUsage(
      [
        frame(1, {
          sessionId: "a",
          turnSeq: 1,
          cumulative: { totalTokens: 10, inputTokens: 8, costUsd: 0.01 },
        }),
        frame(2, {
          sessionId: "a",
          turnSeq: 2,
          cumulative: { totalTokens: 15, inputTokens: 12, costUsd: 0.02 },
        }),
        frame(3, {
          sessionId: "b",
          turnSeq: 1,
          cumulative: { totalTokens: 20, inputTokens: 18, costUsd: 0 },
        }),
      ],
      "selected",
    );
    expect(aggregateSessionUsage(groups)).toEqual({
      counters: { totalTokens: 35, inputTokens: 30, costUsd: 0.02 },
      complete: 2,
    });
  });
  it("does not publish partial numbers for missing or conflicting session snapshots", () => {
    const groups = projectUsage(
      [
        frame(1, {
          sessionId: "a",
          turnSeq: 1,
          cumulative: { totalTokens: 10, inputTokens: 8 },
        }),
        frame(2, {
          sessionId: "b",
          turnSeq: 1,
          cumulative: { inputTokens: 3 },
        }),
        frame(3, {
          sessionId: "c",
          turnSeq: 1,
          cumulative: { totalTokens: 5 },
        }),
        frame(4, {
          sessionId: "c",
          turnSeq: 1,
          cumulative: { totalTokens: 7 },
        }),
      ],
      "selected",
    );
    expect(aggregateSessionUsage(groups)).toEqual({
      counters: {},
      complete: 2,
    });
    expect(aggregateSessionUsage([])).toEqual({ counters: {}, complete: 0 });
  });
});
