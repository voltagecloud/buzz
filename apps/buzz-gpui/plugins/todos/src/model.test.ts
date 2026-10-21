import { describe, expect, it } from "vitest";
import { profileTarget } from "@buzz/plugin-sdk/identity";
import { addTodo, assignTodo, readTodos, setStatus } from "./model.ts";

describe("Canvas todo source edits", () => {
  it("changes only the selected checkbox byte, including duplicate labels and CRLF", () => {
    const content =
      "# Notes\r\nuntouched  \r\n\r\n## Todos\r\n\r\n- [ ] Same\r\n- [X] Same\r\n\r\n## Keep\r\n- [ ] Not a todo\r\n";
    const { items } = readTodos(content);
    expect(items.map((i) => [i.label, i.status])).toEqual([
      ["Same", "todo"],
      ["Same", "done"],
    ]);
    const item = items[1];
    if (!item) throw new Error("Expected second todo");
    const changed = setStatus(content, item.offset, "todo");
    expect(changed).toBe(content.replace("[X]", "[ ]"));
    expect(setStatus(changed, item.offset, "done")).toBe(
      content.replace("[X]", "[x]"),
    );
  });
  it("reads and writes Doing as `[/]` without changing any other byte", () => {
    const content = "## Todos\n\n- [ ] Plan\n* [/] Build · note\n- [x] Ship\n";
    const { items } = readTodos(content);
    expect(items.map((i) => [i.label, i.status])).toEqual([
      ["Plan", "todo"],
      ["Build · note", "doing"],
      ["Ship", "done"],
    ]);
    const [plan, build] = items;
    if (!plan || !build) throw new Error("Expected fixture todos");
    const doing = setStatus(content, plan.offset, "doing");
    expect(doing).toBe(content.replace("- [ ] Plan", "- [/] Plan"));
    expect(setStatus(doing, plan.offset, "todo")).toBe(content);
    expect(setStatus(content, build.offset, "done")).toBe(
      content.replace("* [/]", "* [x]"),
    );
    expect(readTodos("## Todos\n- [-] Other\n- [//] Mark\n").items).toEqual([]);
  });
  it("recognizes actual Markdown structure, not examples or nested task lists", () => {
    const content =
      "```md\n## Todos\n- [ ] Fenced\n```\n\n## Todos\n\n- [ ] Real\n  - [ ] Nested\n\n    [ ] Continuation\n\n> - [ ] Quoted\n\n```\n- [ ] Code\n```\n\n### Details\n\n+ [x] Also real\n\n# Next\n\n- [ ] Other\n";
    expect(readTodos(content).items.map((i) => i.label)).toEqual([
      "Real",
      "Also real",
    ]);
  });
  it("preserves prose, sections, markers and trailing whitespace when adding", () => {
    const before =
      "Preamble  \r\n\r\n## Todos ###\r\n\r\nNotes  \r\n* [x] Old\r\n\r\n## Other\r\nKeep exactly.  ";
    const next = addTodo(before, " New **task** ");
    expect(next).toBe(
      before.replace(
        "## Todos ###",
        "## Todos ###\r\n\r\n- [ ] New **task**\r\n",
      ),
    );
    expect(readTodos(next).items.map((i) => i.label)).toEqual([
      "New **task**",
      "Old",
    ]);
  });
  it.each(["", "# Notes\nKeep me.", "# Notes\r\nKeep me."])(
    "creates one ordinary Todos section without rewriting existing content (%j)",
    (content) => {
      const next = addTodo(content, "First");
      expect(next.startsWith(content)).toBe(true);
      expect(readTodos(next).items.map((i) => i.label)).toEqual(["First"]);
      expect(
        readTodos(addTodo(next, "Second")).items.map((i) => i.label),
      ).toEqual(["Second", "First"]);
    },
  );
  it("rejects ambiguous headings, multiline entries, stale offsets and swallowed insertions", () => {
    expect(() => readTodos("## Todos\n\n## Todos\n")).toThrow(/more than one/);
    expect(() => addTodo("", "a\n## Escape")).toThrow(/single line/);
    expect(() => addTodo("", "   ")).toThrow(/single line/);
    expect(() => setStatus("## Todos\n- [ ] Hi", 0, "done")).toThrow(/changed/);
    expect(() => addTodo("```unterminated\n", "Oops")).toThrow(/safely/);
    expect(() => addTodo("<!-- open comment\n", "Oops")).toThrow(/safely/);
  });
});

function first(content: string) {
  const item = readTodos(content).items[0];
  if (!item) throw new Error("Expected fixture todo");
  return item;
}
const alex = { pubkey: "a".repeat(64), name: "Alex" };
const sam = { pubkey: "b".repeat(64), name: "Sam" };
it("assigns, replaces and clears only the suffix; keeps CRLF, prose, nested lines and trailing spaces", () => {
  const before =
    "# Notes\r\nKeep this.\r\n\r\n## Todos\r\n\r\n* [X] Ship **it**  \r\n  - Keep nested.\r\n\r\n## Decisions\r\nUntouched.";
  const item = first(before);
  const assigned = assignTodo(before, item.offset, alex);
  expect(assigned).toBe(
    before.replace(
      "Ship **it**",
      `Ship **it** · Assignee: [Alex](${profileTarget(alex.pubkey)})`,
    ),
  );
  expect(readTodos(assigned).items[0]?.assignee).toMatchObject(alex);
  expect(setStatus(assigned, item.offset, "doing")).toBe(
    assigned.replace("[X]", "[/]"),
  );
  const changed = assignTodo(assigned, item.offset, sam);
  expect(changed).toBe(
    assigned.replace(
      `[Alex](${profileTarget(alex.pubkey)})`,
      `[Sam](${profileTarget(sam.pubkey)})`,
    ),
  );
  expect(assignTodo(changed, item.offset)).toBe(before);
});
it.each(["A] (B) \\ *C* &amp; <script>", "A\nB\rC", "", "👑 Queen [test]"])(
  "round-trips untrusted profile name %j without changing identity or Markdown structure",
  (name) => {
    const before = "## Todos\n- [ ] First\n- [ ] Second\n";
    const item = first(before);
    const assigned = assignTodo(before, item.offset, { ...alex, name });
    expect(readTodos(assigned).items[0]?.assignee).toMatchObject({
      pubkey: alex.pubkey,
      name: name.replace(/\s+/g, " ").trim() || alex.pubkey,
    });
    expect(assignTodo(assigned, item.offset)).toBe(before);
  },
);
it.each([
  `Task [Alex](${profileTarget(alex.pubkey)})`,
  `Task · Assignee: [Alex](${profileTarget(alex.pubkey)} "title")`,
  `Task · Assignee: [Alex](<${profileTarget(alex.pubkey)}>)`,
  `Task · Assignee: [Alex](https://example.com)`,
  `Task · Assignee: [Alex](nostr:npub1invalid)`,
  `Task · Assignee: [Alex](${profileTarget(alex.pubkey)}) after`,
  `Task \` · Assignee: [Alex](${profileTarget(alex.pubkey)})\``,
  `Task · Assignee: **[Alex](${profileTarget(alex.pubkey)})**`,
])("preserves lookalike prose %s", (label) => {
  const before = `## Todos\n- [ ] ${label}\n`;
  const item = first(before);
  expect(item.assignee).toBeUndefined();
  expect(assignTodo(before, item.offset)).toBe(before);
  const next = assignTodo(before, item.offset, sam);
  expect(readTodos(next).items[0]?.label).toBe(label);
  expect(assignTodo(next, item.offset)).toBe(before);
});
it("refuses swallowed assignments, invalid users and stale offsets", () => {
  const before = "## Todos\n- [ ] Task `unfinished\n  closed`";
  expect(() => assignTodo(before, first(before).offset, alex)).toThrow(
    /safely/,
  );
  expect(() => assignTodo("## Todos\n- [ ] Task", 0, alex)).toThrow(/changed/);
  expect(() =>
    assignTodo(before, first(before).offset, {
      ...alex,
      pubkey: "bad",
    }),
  ).toThrow(/valid/);
});

it("recognizes first-line assignment when continuation prose has another link", () => {
  const before = `## Todos\n- [ ] Task · Assignee: [Alex](${profileTarget(alex.pubkey)})\n  See [docs](https://example.com).\n`;
  const item = first(before);
  expect(item.assignee?.pubkey).toBe(alex.pubkey);
  expect(assignTodo(before, item.offset)).toBe(
    "## Todos\n- [ ] Task\n  See [docs](https://example.com).\n",
  );
});
