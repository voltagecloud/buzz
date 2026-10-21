type CheckCounts = Record<
  | "failing"
  | "cancelled"
  | "pending"
  | "unknown"
  | "skipped"
  | "neutral"
  | "successful",
  number
>;
export type CheckDetail = {
  key: string;
  name: string;
  category: keyof CheckCounts;
  label: string;
  description?: string | undefined;
  url?: string | undefined;
};
export type CheckSummary = {
  state: "failure" | "pending" | "success" | "neutral" | "unavailable";
  label: string;
  description: string;
  checks?: CheckDetail[] | undefined;
};
type CheckRun = {
  status: string;
  conclusion: string | null;
  name?: string;
  details_url?: string;
  html_url?: string;
  output?: { title?: string | null } | null;
};
type CommitStatus = {
  state: string;
  context?: string;
  description?: string | null;
  target_url?: string | null;
};

const resultLabels: Record<string, string> = {
  success: "Passed",
  failure: "Failed",
  error: "Failed",
  timed_out: "Timed out",
  action_required: "Action required",
  stale: "Stale",
  startup_failure: "Startup failed",
  cancelled: "Cancelled",
  skipped: "Skipped",
  neutral: "Neutral",
  queued: "Queued",
  in_progress: "In progress",
  waiting: "Waiting",
  pending: "Pending",
  requested: "Requested",
};
function safeCheckUrl(value: unknown) {
  if (typeof value !== "string") return undefined;
  try {
    const url = new URL(value);
    return url.protocol === "https:" && !url.username && !url.password
      ? url.href
      : undefined;
  } catch {
    return undefined;
  }
}

const scope = "PR head commit · ";
export const unavailableChecks: CheckSummary = {
  state: "unavailable",
  label: "Unavailable",
  description: `${scope}Couldn’t load all checks. Try again or open on GitHub.`,
};

// At most three pages per source (100 results/page), with no background polling.
async function readChecks(
  repository: string,
  sha: string,
  source: "check-runs" | "status",
  signal: AbortSignal,
) {
  const items: (CheckRun | CommitStatus)[] = [];
  for (let page = 1; page <= 3; page++) {
    const response = await fetch(
      `https://api.github.com/repos/${repository}/commits/${encodeURIComponent(sha)}/${source}?per_page=100&page=${page}${source === "check-runs" ? "&filter=latest" : ""}`,
      {
        signal,
        credentials: "omit",
        headers: { Accept: "application/vnd.github+json" },
      },
    );
    if (!response.ok) throw new Error("Checks unavailable");
    const data = await response.json();
    const batch = source === "check-runs" ? data.check_runs : data.statuses;
    if (
      !Array.isArray(batch) ||
      !Number.isInteger(data.total_count) ||
      data.total_count < 0
    )
      throw new Error("Invalid checks response");
    items.push(...batch);
    const more =
      items.length < data.total_count ||
      /rel="next"/.test(response.headers.get("Link") ?? "");
    if (!more) return { items, complete: true };
    if (!batch.length) throw new Error("Incomplete checks response");
  }
  return { items, complete: false };
}

export async function loadGitHubChecks(
  repository: string,
  sha: string,
  signal: AbortSignal,
): Promise<CheckSummary> {
  try {
    const [runs, statuses] = await Promise.all([
      readChecks(repository, sha, "check-runs", signal),
      readChecks(repository, sha, "status", signal),
    ]);
    const counts: CheckCounts = {
      failing: 0,
      cancelled: 0,
      pending: 0,
      unknown: 0,
      skipped: 0,
      neutral: 0,
      successful: 0,
    };
    const checks: CheckDetail[] = [];
    for (const item of [...runs.items, ...statuses.items]) {
      if (!item || typeof item !== "object") throw new Error("Invalid check");
      let category: keyof CheckCounts = "unknown";
      if ("status" in item) {
        if (
          ["queued", "in_progress", "waiting", "pending", "requested"].includes(
            item.status,
          )
        )
          category = "pending";
        else if (item.status === "completed") {
          if (item.conclusion === "success") category = "successful";
          else if (item.conclusion === "cancelled") category = "cancelled";
          else if (item.conclusion === "skipped") category = "skipped";
          else if (item.conclusion === "neutral") category = "neutral";
          else if (
            [
              "failure",
              "timed_out",
              "action_required",
              "stale",
              "startup_failure",
            ].includes(item.conclusion ?? "")
          )
            category = "failing";
        }
      } else if ("state" in item) {
        if (item.state === "success") category = "successful";
        else if (item.state === "pending") category = "pending";
        else if (["failure", "error"].includes(item.state))
          category = "failing";
      }
      counts[category]++;
      const isRun = "status" in item;
      const name = isRun
        ? item.name
        : "context" in item
          ? item.context
          : undefined;
      const description = isRun
        ? item.output?.title
        : "description" in item
          ? item.description
          : undefined;
      const outcome = isRun
        ? item.status === "completed"
          ? item.conclusion
          : item.status
        : "state" in item
          ? item.state
          : undefined;
      checks.push({
        key: `check-${checks.length}`,
        name:
          typeof name === "string" && name.trim()
            ? name
            : isRun
              ? "Unnamed check"
              : "Unnamed status",
        category,
        label:
          category !== "unknown" &&
          typeof outcome === "string" &&
          Object.hasOwn(resultLabels, outcome)
            ? (resultLabels[outcome] ?? "Unknown")
            : "Unknown",
        description:
          typeof description === "string" && description.trim()
            ? description
            : undefined,
        url: isRun
          ? (safeCheckUrl(item.details_url) ?? safeCheckUrl(item.html_url))
          : "target_url" in item
            ? safeCheckUrl(item.target_url)
            : undefined,
      });
    }
    const total = Object.values(counts).reduce((sum, count) => sum + count, 0);
    const complete = runs.complete && statuses.complete;
    const description = total
      ? `${Object.entries(counts)
          .filter(([, count]) => count)
          .map(([category, count]) => `${count} ${category}`)
          .join(", ")} ${total === 1 ? "check" : "checks"}`
      : "No checks reported";
    const state = !complete
      ? "unavailable"
      : counts.failing || counts.cancelled
        ? "failure"
        : counts.pending
          ? "pending"
          : counts.unknown
            ? "unavailable"
            : counts.successful
              ? "success"
              : "neutral";
    const label = !complete
      ? "Incomplete"
      : state === "failure"
        ? "Some not successful"
        : state === "pending"
          ? "Pending"
          : state === "success"
            ? "Successful"
            : state === "unavailable"
              ? "Unknown"
              : total
                ? "No failures"
                : "No checks";
    const priority: (keyof CheckCounts)[] = [
      "failing",
      "cancelled",
      "pending",
      "unknown",
      "successful",
      "skipped",
      "neutral",
    ];
    checks.sort(
      (a, b) => priority.indexOf(a.category) - priority.indexOf(b.category),
    );
    return {
      checks,
      state,
      label,
      description: `${scope}${complete ? "" : "Partial counts (page limit reached): "}${description}.`,
    };
  } catch {
    return unavailableChecks;
  }
}
