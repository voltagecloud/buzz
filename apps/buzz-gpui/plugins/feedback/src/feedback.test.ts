import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { describe, expect, it } from "vitest";
import { diagnostics, feedback, language } from "./feedback.ts";

const image = { name: "shot.png", url: "https://r/media/a.png", mime: "image/png", size: 10, sha256: "a".repeat(64) };
const notes = { name: "feedback-diagnostics.txt", url: "https://r/media/b.txt", mime: "text/plain", size: 9, sha256: "b".repeat(64) };

function setup() {
  const sent: Record<string, unknown>[] = [];
  const asked: string[] = [];
  const host = new FakeHost(feedback)
    .answer("feedback/watch", () => ({ watch: "f1", state: { hydrated: true, available: true, error: null, pending: null } }))
    .answer("feedback/unwatch", () => null)
    .answer("ui/pickFiles", () => ["/tmp/shot.png"])
    .answer("feedback/upload", () => image)
    .answer("feedback/send", (params) => {
      sent.push(params);
      return { diagnostics: params.diagnostics ? notes : null };
    })
    .answer("feedback/retry", (params) => {
      asked.push(`retry ${params.id}`);
      return null;
    })
    .answer("feedback/dismiss", (params) => {
      asked.push(`dismiss ${params.id}`);
      return null;
    })
    .answer("ui/confirm", () => true)
    .answer("ui/closeView", (params) => {
      asked.push(`close ${params.view}`);
      return null;
    });
  return { host, sent, asked };
}

describe("feedback", () => {
  it("adds Send feedback to the account menu", async () => {
    const { host } = setup();
    expect((await host.initialize()).accountActions).toEqual([{ id: "send", title: "Send feedback", icon: "ChatCircle" }]);
  });

  it("sends the text, category, image and diagnostics, then reuses the diagnostics", async () => {
    const { host, sent } = setup();
    await host.initialize();
    const view = await host.open("send", { kind: "dialog" });
    expect(byId(host.tree(view), "send")).toMatchObject({ disabled: true });
    await host.event(view, "bug", "click");
    await host.event(view, "message", "change", "It broke");
    await host.event(view, "image", "click");
    expect(texts(host.tree(view))).toContain("Image uploaded: shot.png");
    await host.event(view, "diagnostics", "toggle", true);
    await host.event(view, "send", "click");
    expect(sent[0]).toMatchObject({ message: "It broke", category: "bug", attachments: [image] });
    expect(sent[0]?.diagnostics).toMatch(/^Buzz feedback diagnostics\ncaptured: /);
    await host.event(view, "send", "click");
    expect(sent[1]?.diagnostics).toEqual(notes);
  });

  it("follows the pending feedback: retry, discard and done", async () => {
    const { host, asked } = setup();
    await host.initialize();
    const view = await host.open("send", { kind: "dialog" });
    const pending = (delivery: string) =>
      host.notify("feedback/changed", {
        watch: "f1",
        state: { hydrated: true, available: true, error: null, pending: { id: "e1", content: "It broke", delivery, error: "offline" } },
      });
    await pending("failed");
    expect(texts(host.tree(view))).toContain("Feedback was not delivered. offline");
    await host.event(view, "retry", "click");
    await host.event(view, "discard", "click");
    await pending("accepted");
    expect(texts(host.tree(view))).toContain("Feedback received by the relay.");
    expect(elements(host.tree(view)).some((element) => element.id === "done")).toBe(true);
    expect(asked).toEqual(["retry e1", "dismiss e1", `close ${view}`]);
  });

  it("writes diagnostics without logs", () => {
    expect(language("en_US.UTF-8")).toBe("en-US");
    expect(language("C")).toBe("unknown");
    const text = diagnostics("1.2.3", new Date(0), { LANG: "de_DE.UTF-8" });
    expect(text).toMatch(/^Buzz feedback diagnostics\ncaptured: 1970-01-01T00:00:00.000Z\napp version: 1.2.3\nplatform: \w+ \w+\nuser agent: buzz-gpui\/1.2.3 \(\w+\)\nlanguage: de-DE$/);
  });
});
