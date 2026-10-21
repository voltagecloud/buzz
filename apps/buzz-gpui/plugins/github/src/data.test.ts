import { assert, afterEach, expect, it, vi } from "vitest";
import { loadGitHubDetails } from "./data.ts";
import { parseGitHubReference } from "./references.ts";

afterEach(() => vi.unstubAllGlobals());
const reference = parseGitHubReference(
  "https://github.com/block/buzz/pull/23#issuecomment-1",
);
assert.exists(reference);
it("loads PR details and preserves merged state, branches, and change counts", async () => {
  const fetch = vi.fn().mockResolvedValue(
    new Response(
      JSON.stringify({
        title: "Simplify panels",
        body: "An independent panel contract.",
        body_html: "<p>An independent panel contract.</p>",
        state: "closed",
        merged: true,
        merged_at: "2026-10-02T12:00:00Z",
        merged_by: { login: "maintainer" },
        user: { login: "author" },
        head: { label: "block:panels", sha: "head-sha" },
        base: { label: "block:main" },
        changed_files: 3,
        comments: 5,
        additions: 10,
        deletions: 80,
      }),
    ),
  );
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  const data = await loadGitHubDetails(reference, signal);
  expect(fetch).toHaveBeenCalledWith(
    "https://api.github.com/repos/block/buzz/pulls/23",
    expect.objectContaining({
      signal,
      credentials: "omit",
      headers: { Accept: "application/vnd.github.full+json" },
    }),
  );
  expect(data).toMatchObject({
    title: "Simplify panels",
    state: "Merged",
    mergedAt: "2026-10-02T12:00:00Z",
    mergedBy: "maintainer",
    mergedByUrl: "https://github.com/maintainer",
    author: "author",
    body: "An independent panel contract.",
    bodyHtml: "<p>An independent panel contract.</p>",
    headSha: "head-sha",
  });
  expect(data.facts.map(([label]) => label)).not.toContain("Comments");
  expect(data.facts).toContainEqual([
    "Changes",
    { additions: 10, deletions: 80 },
  ]);
});
it.each([
  [404, "private or unavailable"],
  [403, "limit"],
  [429, "limit"],
  [500, "500"],
])("explains API failure %s", async (status, message) => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(new Response(null, { status: Number(status) })),
  );
  await expect(
    loadGitHubDetails(reference, new AbortController().signal),
  ).rejects.toThrow(String(message));
});

it("does not expose check head plumbing on non-PR objects", async () => {
  const issue = parseGitHubReference(
    "https://github.com/sample/project/issues/1",
  );
  assert.exists(issue);
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          title: "An issue",
          head: { sha: "not-a-pr" },
        }),
      ),
    ),
  );
  expect(
    (await loadGitHubDetails(issue, new AbortController().signal)).headSha,
  ).toBeUndefined();
});

it.each([
  ["block/buzz", "tho/small-improvement", "tho/small-improvement"],
  ["BLOCK/Buzz", "tho/small-improvement", "tho/small-improvement"],
  ["contributor/buzz", "tho/small-improvement", "block:tho/small-improvement"],
  [
    "block/another-repo",
    "tho/small-improvement",
    "block:tho/small-improvement",
  ],
  [null, "tho/small-improvement", "block:tho/small-improvement"],
  ["block/buzz", undefined, "block:tho/small-improvement"],
])(
  "shortens branches only for a known matching repository: %s, %s",
  async (repository, ref, label) => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            head: {
              label: "block:tho/small-improvement",
              ref,
              repo: repository ? { full_name: repository } : null,
            },
            base: {
              label: "block:main",
              ref: "main",
              repo: { full_name: "block/buzz" },
            },
          }),
        ),
      ),
    );
    const data = await loadGitHubDetails(
      reference,
      new AbortController().signal,
    );
    expect(data.facts).toContainEqual([
      "Branch",
      {
        head: {
          label,
          url:
            repository && ref
              ? `https://github.com/${repository}/tree/${ref}`
              : undefined,
        },
        base: { label: "main", url: "https://github.com/block/buzz/tree/main" },
      },
    ]);
  },
);

it("uses PR creation time for the description and keeps its avatar to the GitHub origin", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            title: "A PR",
            user: {
              login: "author",
              avatar_url: "https://avatars.githubusercontent.com/u/1?v=4",
            },
            created_at: "2026-10-01T12:00:00Z",
            updated_at: "2026-10-02T12:00:00Z",
          }),
        ),
    ),
  );
  const reference = parseGitHubReference(
    "https://github.com/sample/project/pull/1",
  );
  if (!reference) throw new Error("Missing test reference");
  const details = await loadGitHubDetails(
    reference,
    new AbortController().signal,
  );
  expect(details).toMatchObject({
    createdAt: "2026-10-01T12:00:00Z",
    updatedAt: "2026-10-02T12:00:00Z",
    authorAvatar: "https://avatars.githubusercontent.com/u/1?v=4",
  });
});

it("keeps the comment count on issues, where no conversation is rendered", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(new Response(JSON.stringify({ comments: 0 }))),
  );
  const issue = parseGitHubReference(
    "https://github.com/sample/project/issues/1",
  );
  assert.exists(issue);
  const data = await loadGitHubDetails(issue, new AbortController().signal);
  expect(data.facts).toContainEqual(["Comments", 0]);
});

it.each([null, "bad", undefined])(
  "does not invent a merge timestamp from unavailable merged_at: %s",
  async (merged_at) => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              merged: true,
              merged_at,
              updated_at: "2026-10-02T12:00:00Z",
              merged_by: { login: "bad/profile" },
            }),
          ),
      ),
    );
    const data = await loadGitHubDetails(
      reference,
      new AbortController().signal,
    );
    expect(data.state).toBe("Merged");
    expect(data.mergedAt).toBeUndefined();
    expect(data.mergedByUrl).toBeUndefined();
  },
);
