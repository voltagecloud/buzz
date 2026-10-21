import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RpcError } from "@buzz/plugin-sdk";
import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { beforeEach, describe, expect, it } from "vitest";
import { draftKey, parseDraft } from "./panel.ts";
import { todos } from "./todos.ts";

const alice = "a".repeat(64);
const bob = "b".repeat(64);

interface Head {
  id: string;
  createdAt: number;
  content: string;
}

function setup(content: string | null, options: { saveFails?: "conflict" } = {}) {
  let head: Head | null = content === null ? null : { id: "r1", createdAt: 1, content };
  const saves: Params[] = [];
  const host = new FakeHost(todos)
    .answer("canvas/available", () => true)
    .answer("canvas/read", () => head)
    .answer("canvas/save", (params) => {
      saves.push(params);
      if (options.saveFails === "conflict") throw new RpcError(5, "changed");
      head = { id: `r${saves.length + 1}`, createdAt: 2 + saves.length, content: params.content };
      return head;
    })
    .answer("session/watch", () => ({ watch: "w1" }))
    .answer("session/unwatch", () => null)
    .answer("profiles/get", () => [])
    .answer("names/watch", () => ({ watch: "n1", names: { [alice]: { name: "Alice", source: "public-profile" } } }))
    .answer("names/unwatch", () => null)
    .answer("ui/confirm", () => true);
  return { host, saves };
}

type Params = Record<string, unknown> & { content?: string };

async function opened(host: FakeHost) {
  await host.initialize();
  const view = await host.open("todos", { kind: "panel", channel: "c1", scope: "wss://r#me", channelName: "general" });
  await host.notify("session/changed", {
    watch: "w1",
    what: "channelList",
    value: { status: "ready", channels: [{ id: "c1", members: [alice, bob] }] },
  });
  return view;
}

beforeEach(() => {
  process.env.BUZZ_PLUGIN_DATA_DIR = mkdtempSync(join(tmpdir(), "todos-"));
});

describe("todos", () => {
  it("adds a side launcher panel drawn edge to edge", async () => {
    const { host } = setup(null);
    const answer = await host.initialize();
    expect(answer.panels[0]).toMatchObject({ id: "todos", launcher: { placement: "side" }, layout: "workspace" });
  });

  it("lists the Canvas's todos by status and saves a check", async () => {
    const { host, saves } = setup("Intro\n\n## Todos\n\n- [ ] Write\n- [/] Test\n- [x] Ship\n");
    const view = await opened(host);
    const tree = host.tree(view);
    expect(texts(tree)).toEqual(expect.arrayContaining(["Todos", "To do", "In progress", "Done"]));
    const write = elements(tree).find((element) => element.type === "checkbox" && element.label === "Write");
    expect(write).toMatchObject({ checked: false, indeterminate: false });
    expect(elements(tree).find((element) => element.label === "Test")).toMatchObject({ indeterminate: true });
    expect(elements(tree).find((element) => element.label === "Ship")).toMatchObject({ checked: true, completed: true });
    await host.event(view, write?.id as string, "toggle", true);
    await new Promise((resolve) => setTimeout(resolve, 1100));
    await host.settle();
    expect(saves).toEqual([
      { channel: "c1", content: "Intro\n\n## Todos\n\n- [x] Write\n- [/] Test\n- [x] Ship\n", expected: "r1" },
    ]);
  });

  it("adds a todo from the field and starts a section when there is none", async () => {
    const { host, saves } = setup("Notes");
    const view = await opened(host);
    expect(texts(host.tree(view))).toContain("No todos yet");
    await host.event(view, "new", "change", "Plan the launch");
    await host.event(view, "add", "click");
    await new Promise((resolve) => setTimeout(resolve, 1100));
    await host.settle();
    expect(saves[0]?.content).toBe("Notes\n\n## Todos\n\n- [ ] Plan the launch\n");
    expect(byId(host.tree(view), "new")).toMatchObject({ value: "" });
  });

  it("assigns a member by name and offers each member with their picture", async () => {
    const { host, saves } = setup("## Todos\n\n- [ ] Write\n");
    const view = await opened(host);
    const menu = elements(host.tree(view)).find((element) => element.type === "menu" && element.title === "Assign to");
    expect(menu?.options).toEqual(
      expect.arrayContaining([
        { value: "", label: "Unassigned", icon: "User" },
        expect.objectContaining({ value: alice, avatar: alice, label: expect.stringMatching(/^Alice · npub…/) }),
      ]),
    );
    await host.event(view, menu?.id as string, "change", alice);
    await new Promise((resolve) => setTimeout(resolve, 1100));
    await host.settle();
    expect(saves[0]?.content).toMatch(/^## Todos\n\n- \[ \] Write · Assignee: \[Alice\]\(nostr:npub1/);
  });

  it("keeps the draft and says so when the Canvas changed elsewhere", async () => {
    const { host, saves } = setup("## Todos\n\n- [ ] Write\n", { saveFails: "conflict" });
    const view = await opened(host);
    const write = elements(host.tree(view)).find((element) => element.type === "checkbox");
    await host.event(view, write?.id as string, "toggle", true);
    await new Promise((resolve) => setTimeout(resolve, 1100));
    await host.settle();
    expect(saves).toHaveLength(1);
    expect(texts(host.tree(view))).toContain(
      "Canvas changed elsewhere. Refresh to load the latest todos. Your changes are kept until then.",
    );
    expect(byId(host.tree(view), "conflict-refresh")).toBeDefined();
  });

  it("reads saved drafts in buzz-app's shape", () => {
    expect(parseDraft({ content: "x", original: "x", base: "abc", input: "" })?.base).toBe("abc");
    expect(parseDraft({ content: "x", original: "x", base: null })).toBeUndefined();
    expect(parseDraft({ content: "x", original: "x", base: 1, input: "" })).toBeUndefined();
    expect(parseDraft(null)).toBeUndefined();
    expect(draftKey("c1")).toBe("todos-draft-v1:c1");
  });
});
