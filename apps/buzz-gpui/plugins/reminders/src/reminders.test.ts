import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { beforeEach, describe, expect, it } from "vitest";
import { reminders } from "./reminders.ts";

const now = () => Math.floor(Date.now() / 1000);
const community = { status: "ready", scope: "wss://r#me", relay: "wss://r", live: true, liveStatus: { roster: "verified" } };
const reminder = (id: string, notBefore: number, fields: object = {}) => ({
  id,
  eventId: `e-${id}`,
  createdAt: notBefore - 100,
  notBefore,
  status: "pending",
  note: null,
  message: { id: `m-${id}`, channel: "c1", preview: `Message ${id}`, author: "a".repeat(64) },
  ...fields,
});

function setup() {
  const submitted: Record<string, unknown>[] = [];
  const calls: string[] = [];
  const host = new FakeHost(reminders)
    .answer("community/current", () => community)
    .answer("reminders/watch", () => ({ watch: "r1" }))
    .answer("session/watch", () => ({ watch: "s1" }))
    .answer("notifications/submit", (params) => {
      submitted.push(params);
      return true;
    })
    .answer("text/plain", (params) => String(params.text).replace(/\*/g, ""))
    .answer("ui/openDialog", (params) => {
      calls.push(`dialog ${JSON.stringify(params.context)}`);
      return { view: "d1" };
    })
    .answer("reminders/create", (params) => {
      calls.push(`create ${params.message} ${params.preview}`);
      return null;
    })
    .answer("reminders/complete", (params) => {
      calls.push(`complete ${params.id}`);
      return null;
    })
    .answer("ui/closeView", () => null);
  const list = (items: object[]) =>
    host.notify("reminders/changed", { watch: "r1", status: "ready", hydrated: true, error: null, reminders: items });
  const channels = () =>
    host.notify("session/changed", {
      watch: "s1",
      what: "channelList",
      value: { status: "ready", channels: [{ id: "c1", name: "general", kind: "stream" }] },
    });
  return { host, submitted, calls, list, channels };
}

beforeEach(() => {
  process.env.BUZZ_PLUGIN_DATA_DIR = mkdtempSync(join(tmpdir(), "reminders-"));
});

describe("reminders", () => {
  it("adds the page, the notification category and Remind me", async () => {
    const { host } = setup();
    const answer = await host.initialize();
    expect(answer.pages).toEqual([{ id: "reminders", title: "Reminders", icon: "Alarm", layout: "workspace" }]);
    expect(answer.notifications).toEqual([{ id: "reminders", label: "Reminders" }]);
    expect(answer.messageActions).toEqual([{ id: "remind-me", title: "Remind me", icon: "Alarm" }]);
  });

  it("badges and marks pending reminders and notifies once when one comes due", async () => {
    const { host, submitted, list, channels } = setup();
    await host.initialize();
    await host.settle();
    await channels();
    // Due before the baseline: it stays on the page, without a notification.
    await list([reminder("old", now() - 3600), reminder("later", now() + 3600)]);
    expect(submitted).toEqual([]);
    expect(host.sent).toContainEqual({ method: "page/badge", params: { page: "reminders", text: "1" } });
    expect(host.sent).toContainEqual({ method: "message/markers", params: { action: "remind-me", messages: ["m-later", "m-old"] } });
    // One that comes due after the baseline notifies, once.
    await new Promise((resolve) => setTimeout(resolve, 1100));
    const due = reminder("now", now());
    await list([reminder("old", now() - 3600), due]);
    await list([reminder("old", now() - 3600), due]);
    expect(submitted).toHaveLength(1);
    expect(submitted[0]).toMatchObject({
      category: "reminders",
      channel: "c1",
      message: "m-now",
      community: "wss://r",
      title: "Reminder due · #general",
      body: "Message now",
    });
  });

  it("lists reminders by when they are due and completes one", async () => {
    const { host, calls, list, channels } = setup();
    await host.initialize();
    await host.settle();
    await channels();
    await list([reminder("old", now() - 3600), reminder("done", now() - 7200, { status: "done", notBefore: null })]);
    const view = await host.open("reminders");
    expect(texts(host.tree(view))).toEqual(expect.arrayContaining(["Reminders", "Overdue", "Message old"]));
    expect(byId(host.tree(view), "done-group")).toMatchObject({ label: "▸ Done · 1" });
    await host.event(view, "done:old", "click");
    expect(calls).toContain("complete old");
  });

  it("remind me opens a dialog for the message and saves a preset", async () => {
    const { host, calls } = setup();
    await host.initialize();
    await host.notify("message/action", {
      action: "remind-me",
      message: { id: "m1", channel: "c1", content: "**Ship** it", author: "b".repeat(64) },
      community: "wss://r",
    });
    expect(calls[0]).toBe(`dialog {"message":"m1","channel":"c1","preview":"Ship it","author":"${"b".repeat(64)}"}`);
    const view = await host.open("remind", {
      kind: "dialog",
      context: { message: "m1", channel: "c1", preview: "Ship it", author: "b".repeat(64) },
    });
    expect(elements(host.tree(view)).filter((element) => element.type === "button").map((button) => button.label)).toContain(
      "In 1 hour",
    );
    await host.event(view, "preset:1", "click");
    expect(calls).toContain("create m1 Ship it");
  });
});
