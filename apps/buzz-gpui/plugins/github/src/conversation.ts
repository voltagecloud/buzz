// A pull request's conversation: discussion comments and submitted
// reviews, paged from GitHub's public API (buzz-app
// `bundled/github/conversation.ts`, without its React hook).

import type { GitHubDetails } from "./data.ts";

export type ConversationMessage = {
  author: string;
  authorUrl?: string | undefined;
  authorAvatar?: string | undefined;
  createdAt?: string | undefined;
  body: string;
  bodyHtml?: string | undefined;
};

export type ConversationEntry = ConversationMessage & {
  id: number;
  state?: string | undefined;
};
export type ConversationEvent = {
  key: string;
  message: ConversationEntry;
  kind: "discussion" | "review" | "merge";
};
export type ConversationSource = "discussion" | "reviews";
const pageSize = 30;

function endpoint(url: string, source: ConversationSource) {
  const path = new URL(url).pathname.replace(
    "/pull/",
    source === "discussion" ? "/issues/" : "/pulls/",
  );
  return `https://api.github.com/repos${path}/${source === "reviews" ? "reviews" : "comments"}`;
}

type EntryData = {
  id: number;
  user?: { login?: string; avatar_url?: string } | null;
  body?: string | null;
  body_html?: string | null;
  created_at?: string;
  submitted_at?: string | null;
  state?: string;
};

export async function loadConversationPage(
  url: string,
  source: ConversationSource,
  page: number,
  signal: AbortSignal,
) {
  const base = endpoint(url, source);
  const request = new URL(base);
  request.searchParams.set("per_page", String(pageSize));
  request.searchParams.set("page", String(page));
  const response = await fetch(request.href, {
    signal,
    credentials: "omit",
    headers: { Accept: "application/vnd.github.full+json" },
  });
  if (!response.ok)
    throw new Error(
      response.status === 403 || response.status === 429
        ? "GitHub’s public API limit was reached. Try again later."
        : `GitHub couldn’t load this source (${response.status}).`,
    );
  const data: EntryData[] = await response.json();
  if (!Array.isArray(data))
    throw new Error("GitHub returned an unreadable conversation page.");
  const entries = data
    .filter(
      (entry) =>
        Number.isSafeInteger(entry?.id) &&
        (source !== "reviews" || !!entry.submitted_at),
    )
    .map((entry): ConversationEntry => {
      const author = entry.user?.login ?? "";
      const date = source === "reviews" ? entry.submitted_at : entry.created_at;
      return {
        id: entry.id,
        author,
        authorUrl: /^[a-z0-9-]+$/i.test(author)
          ? `https://github.com/${encodeURIComponent(author)}`
          : undefined,
        authorAvatar: /^https:\/\/avatars\.githubusercontent\.com\//.test(
          entry.user?.avatar_url ?? "",
        )
          ? entry.user?.avatar_url
          : undefined,
        createdAt: date && Number.isFinite(Date.parse(date)) ? date : undefined,
        body: entry.body ?? "",
        bodyHtml: entry.body_html ?? undefined,
        state: entry.state,
      };
    });
  // The Link header tells us whether another page exists; never follow arbitrary URLs.
  const nextLink = response.headers
    .get("link")
    ?.split(",")
    .find((link) => /;\s*rel="next"/.test(link));
  let next: number | undefined;
  if (nextLink) {
    const link = /^\s*<([^>]+)>/.exec(nextLink)?.[1];
    if (!link) throw new Error("GitHub returned an unreadable next-page link.");
    const parsed = new URL(link);
    const candidate = Number(parsed.searchParams.get("page"));
    // GitHub canonicalizes owner/name links to repository IDs. Read only the
    // cursor; subsequent requests still use our trusted owner/name endpoint.
    const canonicalPath = parsed.pathname.replace(
      /^\/repositories\/[1-9]\d*(?=\/)/,
      request.pathname.split("/").slice(0, 4).join("/"),
    );
    if (
      parsed.origin !== request.origin ||
      canonicalPath !== request.pathname ||
      parsed.username ||
      parsed.password ||
      parsed.hash ||
      parsed.searchParams.getAll("page").length !== 1 ||
      candidate !== page + 1
    )
      throw new Error("GitHub returned an unexpected next-page link.");
    next = candidate;
  }
  return { entries, next };
}

function chronological(a: ConversationEntry, b: ConversationEntry) {
  return (
    (Date.parse(a.createdAt ?? "") || 0) -
      (Date.parse(b.createdAt ?? "") || 0) || a.id - b.id
  );
}

export function conversationEvents(
  discussion: ConversationEntry[],
  reviews: ConversationEntry[],
  details?: GitHubDetails,
): ConversationEvent[] {
  return [
    ...(details?.mergedAt
      ? [
          {
            key: "merge",
            kind: "merge" as const,
            message: {
              id: 0,
              author: details.mergedBy ?? "",
              authorUrl: details.mergedByUrl,
              createdAt: details.mergedAt,
              body: "",
            },
          },
        ]
      : []),
    ...discussion.map(
      (message): ConversationEvent => ({
        key: `discussion-${message.id}`,
        message,
        kind: "discussion",
      }),
    ),
    // This conversation-level pane deliberately omits inline code threads:
    // loading and grouping them adds an API request and code-level UI. Leave
    // that fuller flow on GitHub; empty comment-only reviews have no summary
    // to show here. Keep the source entries intact so paging still works.
    ...reviews
      .filter(
        (message) => message.state !== "COMMENTED" || !!message.body.trim(),
      )
      .map(
        (message): ConversationEvent => ({
          key: `review-${message.id}`,
          message,
          kind: "review",
        }),
      ),
  ].sort(
    (a, b) => chronological(a.message, b.message) || a.key.localeCompare(b.key),
  );
}

export type EventRun = {
  key: string;
  kind: "history";
  events: ConversationEvent[];
  afterMerge?: boolean;
};

/** Keep milestones visible; fold conversation stretches without crossing the merge event. */
export function groupConversationEvents(
  events: ConversationEvent[],
): (ConversationEvent | EventRun)[] {
  const merge = events.find((event) => event.kind === "merge");
  const mergedAt = Date.parse(merge?.message.createdAt ?? "");
  const latestDecisions = new Map<string, number>();
  const superseded = new Set<string>();
  for (const event of [...events].reverse()) {
    // Dismissals are review history too; comments/unknown states never replace a verdict.
    if (
      event.kind !== "review" ||
      !["APPROVED", "CHANGES_REQUESTED", "DISMISSED"].includes(
        event.message.state ?? "",
      )
    )
      continue;
    const time = Date.parse(event.message.createdAt ?? "");
    if (!Number.isFinite(time)) continue;
    const author = event.message.author.toLowerCase();
    if (
      time < mergedAt ||
      (author && time < (latestDecisions.get(author) ?? time))
    )
      superseded.add(event.key);
    if (author)
      latestDecisions.set(
        author,
        Math.max(time, latestDecisions.get(author) ?? time),
      );
  }
  const grouped: (ConversationEvent | EventRun)[] = [];
  let run: ConversationEvent[] = [];
  let afterMerge = false;
  const flush = () => {
    const first = run[0];
    const approvalsOnly = run.every(
      (event) =>
        event.kind === "review" &&
        event.message.state === "APPROVED" &&
        Date.parse(event.message.createdAt ?? "") < mergedAt,
    );
    if (
      first &&
      !approvalsOnly &&
      (run.length > 1 || superseded.has(first.key) || afterMerge)
    )
      grouped.push({
        key: first.key,
        kind: "history",
        events: run,
        ...(afterMerge ? { afterMerge: true } : {}),
      });
    else grouped.push(...run);
    run = [];
  };
  events.forEach((event, index) => {
    const comment =
      event.kind === "discussion" ||
      (event.kind === "review" && event.message.state === "COMMENTED");
    const postMerge =
      comment && Date.parse(event.message.createdAt ?? "") > mergedAt;
    if (
      postMerge ||
      (index < events.length - 1 && (comment || superseded.has(event.key)))
    ) {
      if (run.length && afterMerge !== postMerge) flush();
      afterMerge = postMerge;
      run.push(event);
    } else {
      flush();
      grouped.push(event);
    }
  });
  flush();
  return grouped;
}
