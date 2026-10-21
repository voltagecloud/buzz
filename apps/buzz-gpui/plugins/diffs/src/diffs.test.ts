import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { describe, expect, it } from "vitest";
import { diffs } from "./diffs.ts";
import { expandTabs, header, splitRows } from "./view.ts";

const patch = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n";
const message = (fields: object = {}) => ({
  id: "m1",
  kind: 40008,
  content: patch,
  diff: { filePath: "a.ts", repoUrl: "https://github.com/o/r", commitSha: "abcdef1234", description: "Fix it", truncated: false },
  ...fields,
});

describe("diffs", () => {
  it("draws a diff message as a card with its lines and leaves others", async () => {
    const host = new FakeHost(diffs);
    const answer = await host.initialize();
    expect(answer.messageRenderers).toEqual([{ id: "diff", title: "Code diff", kinds: [40008], order: 0 }]);
    expect(await host.buzz.request("message/render", { renderer: "diff", message: { id: "x", content: "hi" } })).toBeNull();
    const { tree } = await host.buzz.request<{ tree: never }>("message/render", { renderer: "diff", message: message() });
    expect(texts(tree)).toContain("a.ts");
    expect(texts(tree)).toContain("Fix it");
    const link = elements(tree).find((element) => element.type === "link");
    expect(link).toMatchObject({ label: "abcdef1", url: "https://github.com/o/r/commit/abcdef1234" });
    const code = elements(tree).find((element) => element.type === "code");
    expect(code?.lines).toEqual([
      { cells: [{ kind: "number", text: "1", tone: "context" }, { kind: "number", text: "1", tone: "context" }, { text: "ctx", tone: "context" }] },
      { cells: [{ kind: "number", text: "2", tone: "removed" }, { kind: "number", text: "", tone: "removed" }, { text: "old", tone: "removed" }] },
      { cells: [{ kind: "number", text: "", tone: "added" }, { kind: "number", text: "2", tone: "added" }, { text: "new", tone: "added" }] },
    ]);
  });

  it("shows a patch it cannot read as raw text", async () => {
    const host = new FakeHost(diffs);
    await host.initialize();
    const { tree } = await host.buzz.request<{ tree: never }>("message/render", {
      renderer: "diff",
      message: message({ content: "not\ta patch\n" }),
    });
    const code = elements(tree).find((element) => element.type === "code");
    expect(code).toMatchObject({ wrap: false, lines: [{ text: "not     a patch" }] });
  });

  it("expands into a dialog that switches to split", async () => {
    const host = new FakeHost(diffs);
    let opened: unknown;
    host.answer("ui/openDialog", (params) => {
      opened = params;
      return { view: "d1" };
    });
    await host.initialize();
    await host.buzz.request("message/render", { renderer: "diff", message: message() });
    await host.notify("render/event", { renderer: "diff", target: "m1:0", message: "m1", id: "expand", event: "click" });
    expect(opened).toEqual({ id: "expanded", title: "a.ts", size: "expanded", context: { message: "m1" } });
    const view = await host.open("expanded", { kind: "dialog", context: { message: "m1" } });
    expect(byId(host.tree(view), "unified")).toMatchObject({ variant: "subtle" });
    await host.event(view, "split", "click");
    expect(byId(host.tree(view), "split")).toMatchObject({ variant: "subtle" });
    const code = elements(host.tree(view)).find((element) => element.type === "code");
    expect(code).toMatchObject({ minWidth: 768 });
    expect((code?.lines as unknown[])[1]).toEqual({
      cells: [
        { kind: "number", text: "2", tone: "removed" },
        { text: "old", tone: "removed" },
        { kind: "number", text: "2", tone: "added" },
        { text: "new", tone: "added" },
      ],
    });
  });
});

describe("view", () => {
  it("links the commit only over credential-free https", () => {
    expect(header({ repoUrl: "https://x.test/r/", commitSha: "abcdef0" })).toEqual({
      title: "Diff",
      source: "https://x.test/r/commit/abcdef0",
      commitLabel: "abcdef0",
    });
    expect(header({ repoUrl: "https://u:p@x.test/r", commitSha: "abc" })).toEqual({ title: "Diff", commitLabel: "abc" });
    expect(header({ repoUrl: "https://x.test/r", commitSha: "nothex!" }).source).toBe("https://x.test/r");
    expect(header({ repoUrl: "https://x.test/r" }).commitLabel).toBe("Source");
  });

  it("pairs a delete with the insert after it", () => {
    const d = { type: "delete", content: "a", lineNumber: 1, isDelete: true } as const;
    const i = { type: "insert", content: "b", lineNumber: 1, isInsert: true } as const;
    expect(splitRows([d, i, i])).toEqual([
      [d, i],
      [undefined, i],
    ]);
  });

  it("expands tabs to 8-column stops", () => {
    expect(expandTabs("ab\tc\n\td")).toBe("ab      c\n        d");
  });
});
