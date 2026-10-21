import { readFileSync } from "node:fs";
import { join } from "node:path";
import { byId, elements, FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { afterEach, describe, expect, it, vi } from "vitest";
import { github, LINKS } from "./github.ts";
import { parseGitHubReference } from "./references.ts";

const fixtures = join(import.meta.dirname, "../test/fixtures");
const fixture = (name: string) => readFileSync(join(fixtures, `${name}.json`), "utf8");

/** Answers GitHub's API from the fixtures, as the Rust port's dev mode did. */
function serve(): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: string) => {
      const url = new URL(input);
      asked.push(url.pathname);
      const name = url.pathname.endsWith("/comments")
        ? "comments"
        : url.pathname.endsWith("/reviews")
          ? "reviews"
          : url.pathname.endsWith("/check-runs")
            ? "runs"
            : url.pathname.endsWith("/status")
              ? "status"
              : /\/pulls\/\d+$/.test(url.pathname)
                ? "pull"
                : /\/issues\/\d+$/.test(url.pathname)
                  ? "issue"
                  : /\/commits\//.test(url.pathname)
                    ? "commit"
                    : "repo";
      return new Response(fixture(name), { status: 200, headers: { "Content-Type": "application/json" } });
    }),
  );
  return asked;
}

afterEach(() => vi.unstubAllGlobals());

function host() {
  return new FakeHost(github)
    .answer("document/preview", (params) => ({
      text: String(params.text).split("\n")[0],
      images: [],
      textOnly: !/[!<]/.test(String(params.text)),
      line: String(params.text).includes("\n") ? null : params.text,
    }))
    .answer("clipboard/write", () => null)
    .answer("ui/toast", () => null);
}

const wait = async (host: FakeHost) => {
  for (let turn = 0; turn < 10; turn++) await host.settle();
};

describe("github", () => {
  it("offers a panel for exactly the links buzz-app's parser reads", async () => {
    const answer = await host().initialize();
    expect(answer.panels).toEqual([{ id: "github", title: "GitHub", icon: "GithubLogo", pattern: LINKS }]);
    // JavaScript has no `(?i:…)`; spell the host's cases out instead.
    const pattern = new RegExp(LINKS.replace("(?i:github\\.com)", "[Gg][Ii][Tt][Hh][Uu][Bb]\\.[Cc][Oo][Mm]"));
    for (const link of [
      "https://github.com/block/buzz",
      "https://github.com/block/buzz/",
      "https://github.com/block/buzz/pull/23",
      "https://github.com/block/buzz/pull/23#issuecomment-1",
      "https://github.com/block/buzz/pull/23#pullrequestreview-9",
      "https://github.com/block/buzz/issues/4",
      "https://github.com/block/buzz/commit/abcdef1",
      "https://GitHub.com/block/buzz/pull/23?tab=files",
      "https://github.com/block/buzz/pull/23#discussion_r1",
      "https://github.com/block/buzz/issues/4#issuecomment-1",
      "https://github.com/block/buzz/tree/main",
      "https://github.com/block",
      "http://github.com/block/buzz",
      "https://github.com.evil/block/buzz",
      "https://user@github.com/block/buzz",
      "https://github.com/block/buzz/commit/xyz",
    ])
      expect(pattern.test(link), link).toBe(!!parseGitHubReference(link));
  });

  it("shows a pull request with its discussion, then its checks", async () => {
    const asked = serve();
    const panel = host();
    await panel.initialize();
    const view = await panel.open("github", { kind: "panel", target: "https://github.com/block/buzz/pull/23" });
    await wait(panel);
    const tree = panel.tree(view);
    expect(texts(tree)).toContain("Pull request");
    expect(
      elements(tree).find((element) => element.type === "link" && String(element.label).startsWith("feat: add mobile pairing")),
    ).toBeDefined();
    expect(byId(tree, "tabs")).toMatchObject({ value: "discussion" });
    const messages = elements(tree).filter((element) => element.type === "collapsible");
    expect(messages[0]).toMatchObject({ id: "message:description" });
    // Earlier messages fold into a run that opens.
    const run = elements(tree).find((element) => String(element.id).startsWith("run:"));
    expect(run?.label).toMatch(/earlier messages ·/);
    await panel.event(view, String(run?.id), "click");
    expect(elements(panel.tree(view)).filter((element) => element.type === "collapsible").length).toBeGreaterThan(messages.length);
    expect(asked.some((path) => path.endsWith("/comments"))).toBe(true);
    await panel.event(view, "tabs", "change", "checks");
    await wait(panel);
    expect(asked.some((path) => path.endsWith("/check-runs"))).toBe(true);
    expect(elements(panel.tree(view)).some((element) => element.type === "ring" || element.type === "icon")).toBe(true);
  });

  it("opens a message, and copies its link", async () => {
    serve();
    const panel = host();
    await panel.initialize();
    const view = await panel.open("github", { kind: "panel", target: "https://github.com/block/buzz/pull/23" });
    await wait(panel);
    const run = elements(panel.tree(view)).find((element) => String(element.id).startsWith("run:"));
    await panel.event(view, String(run?.id), "click");
    const comment = elements(panel.tree(view)).find(
      (element) => element.type === "collapsible" && String(element.id).startsWith("message:discussion-"),
    );
    expect(comment).toBeDefined();
    await panel.event(view, String(comment?.id), "toggle", true);
    const open = byId(panel.tree(view), String(comment?.id));
    expect(open).toMatchObject({ open: true });
    const copy = (open?.footer as { id: string }[])[0];
    let copied: unknown;
    panel.answer("clipboard/write", (params) => {
      copied = params.text;
      return null;
    });
    await panel.event(view, String(copy?.id), "click");
    expect(copied).toMatch(/^https:\/\/github\.com\/block\/buzz\/pull\/23#issuecomment-\d+$/);
  });

  it("says when GitHub cannot be read, with Retry", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { status: 403 })));
    const panel = host();
    await panel.initialize();
    const view = await panel.open("github", { kind: "panel", target: "https://github.com/block/buzz/issues/4" });
    await wait(panel);
    expect(byId(panel.tree(view), "retry")).toBeDefined();
    expect(texts(panel.tree(view)).join(" ")).toMatch(/limit|couldn/i);
  });
});
