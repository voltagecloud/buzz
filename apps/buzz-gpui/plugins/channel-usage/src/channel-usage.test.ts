import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { describe, expect, it } from "vitest";
import { channelUsage } from "./channel-usage.ts";

const me = "c".repeat(64);
const agent = "a".repeat(64);
const frame = (seq: number, total: number) => ({
  id: `r${seq}`,
  agent,
  createdAt: 100 + seq,
  receivedAt: 100 + seq,
  frame: {
    harness: "goose",
    model: "m1",
    channelId: "c1",
    sessionId: "s1",
    turnSeq: seq,
    timestamp: new Date(1_000_000 + seq * 1000).toISOString(),
    turn: { totalTokens: 10, costUsd: 0.001 },
    cumulative: { totalTokens: total, inputTokens: total - 5, outputTokens: 5, costUsd: total / 1000 },
  },
});

function setup(channel: object = {}) {
  const reads: unknown[] = [];
  const host = new FakeHost(channelUsage)
    .answer("community/current", () => ({ status: "ready", scope: "wss://r#me", viewer: me, cached: false }))
    .answer("session/watch", () => ({ watch: "w1" }))
    .answer("session/unwatch", () => null)
    .answer("archive/settings", () => ({ revision: 7 }))
    .answer("archive/read", (params) => {
      reads.push(params);
      return { agents: [agent], records: [frame(2, 40), frame(1, 20)], before: null, skipped: 0, revision: 7 };
    })
    .answer("names/watch", () => ({ watch: "n1", names: { [agent]: { name: "Goose", source: "agent-directory" } } }))
    .answer("names/unwatch", () => null);
  const open = async () => {
    await host.initialize();
    const view = await host.open("usage", { kind: "panel", channel: "c1", scope: "wss://r#me", viewer: me });
    await host.notify("session/changed", {
      watch: "w1",
      what: "channelList",
      value: { status: "ready", cached: false, channels: [{ id: "c1", members: [me, agent], ...channel }] },
    });
    return view;
  };
  return { host, reads, open };
}

describe("channel usage", () => {
  it("is offered in the menu of channels you can use while agent activity is saved", async () => {
    const { host } = setup();
    const answer = await host.initialize();
    expect(answer.panels[0].menu).toEqual({ label: "View channel usage", member: true, writable: true, archive: true });
  });

  it("totals an agent's sessions and lists its turns", async () => {
    const { host, reads, open } = setup();
    const view = await open();
    expect(reads).toEqual([{ kind: 44200 }]);
    const picker = byId(host.tree(view), "agent");
    expect(picker?.options).toEqual([{ value: agent, label: "Goose · 40 tokens" }]);
    await host.event(view, "agent", "change", agent);
    const shown = texts(host.tree(view));
    expect(shown).toContain("Session usage · Goose");
    expect(shown).toContain("Totals across 1 session");
    expect(shown).toContain("Recorded turns · 2 · newest first");
    expect(shown).toContain("$0.04");
    await host.event(view, "turn:r2", "click");
    expect(byId(host.tree(view), "provenance:r2")).toMatchObject({ label: "▸ Identifiers and provenance" });
    expect(texts(host.tree(view))).toContain("Provider total tokens");
  });

  it("shows nothing in a read-only channel", async () => {
    const { host, reads, open } = setup({ readOnly: true });
    const view = await open();
    expect(reads).toEqual([]);
    expect(elements(host.tree(view))).toHaveLength(1);
  });
});
