import { expect, it } from "vitest";
import { parseGitHubReference } from "./references.ts";

it("recognizes typed GitHub references while refusing lookalike hosts and invalid targets", () => {
  for (const [path, kind] of [
    ["block/buzz", "repository"],
    ["block/buzz/pull/23", "pull"],
    ["block/buzz/issues/12", "issue"],
    ["block/buzz/commit/abcdef1234", "commit"],
  ]) {
    expect(parseGitHubReference(`https://github.com/${path}`)?.kind).toBe(kind);
  }
  for (const url of [
    "https://github.com.evil.test/block/buzz/pull/23",
    "https://someone@github.com/block/buzz",
    "http://github.com/block/buzz",
    "https://github.com/block/buzz/tree/main",
    "https://github.com/block/buzz/blob/main/README.md",
    "https://github.com/block/buzz/pull/0",
    "https://github.com/block/buzz/commit/nope",
  ])
    expect(parseGitHubReference(url)).toBeUndefined();
});

it("claims only PR and issue links whose target the pane can show", () => {
  const pull = "https://github.com/block/buzz-app/pull/838";
  for (const fragment of ["", "#issuecomment-123", "#pullrequestreview-456"])
    expect(parseGitHubReference(`${pull}${fragment}`)?.url).toBe(
      `${pull}${fragment}`,
    );
  expect(
    parseGitHubReference("https://github.com/block/buzz-app/issues/12")?.url,
  ).toBe("https://github.com/block/buzz-app/issues/12");
  for (const url of [
    `${pull}#discussion_r4240253516`,
    `${pull}#event-987`,
    `${pull}#partial-timeline`,
    `${pull}#issuecomment-`,
    `${pull}#pullrequestreview-abc`,
    `${pull}/files`,
    `${pull}/files#diff-0123abcd`,
    `${pull}/files#r4240253516`,
    `${pull}/files/abcdef1..0123456`,
    `${pull}/commits`,
    `${pull}/commits/abcdef1`,
    `${pull}/commits/abcdef1#r4240253516`,
    `${pull}/checks`,
    `${pull}/checks?check_run_id=1`,
    `${pull}/conflicts`,
    `${pull}/changes`,
    `${pull}/activity`,
    "https://github.com/block/buzz-app/issues/12#issuecomment-123",
  ])
    expect(parseGitHubReference(url), url).toBeUndefined();
});
