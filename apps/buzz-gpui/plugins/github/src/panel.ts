// The GitHub panel (buzz-app `bundled/github/index.tsx`): a repository,
// pull request, issue or commit link's identity, state, author and facts,
// its body, and for pull requests the Discussion and Checks tabs. Public
// data only, read from GitHub's REST API without credentials.

import { type Connection, type Element, el, log } from "@buzz/plugin-sdk";
import { loadGitHubChecks, unavailableChecks } from "./checks.ts";
import { type ChecksState, checksView, loading } from "./checks-view.ts";
import type { ConversationMessage } from "./conversation.ts";
import { type GitHubDetails, loadGitHubDetails } from "./data.ts";
import { type GitHubReference, parseGitHubReference } from "./references.ts";
import { Source } from "./source.ts";
import { local, relative } from "./time.ts";
import { type Preview, type TimelineState, timeline } from "./timeline.ts";

const labels = { repository: "Repository", pull: "Pull request", issue: "Issue", commit: "Commit" };
const icons = { repository: "FolderSimple", pull: "GitPullRequest", issue: "GitHubIssue", commit: "GitCommit" };
const stateIcons: Record<string, string> = {
  open: "GitPullRequest",
  draft: "CircleDashed",
  closed: "XCircle",
  merged: "GitMerge",
};
const stateTones = { open: "success", draft: "default", closed: "default", merged: "accent" } as const;

/** `text-transform: capitalize`. */
const capitalize = (text: string) =>
  text
    .split(" ")
    .map((word) => (word ? word[0]?.toUpperCase() + word.slice(1) : word))
    .join(" ");

export class GitHubPanel {
  private reference: GitHubReference | undefined;
  private result: GitHubDetails | string | undefined;
  private attempt = 0;
  private controller = new AbortController();
  private tab: "discussion" | "checks" = "discussion";
  private discussion: Source | undefined;
  private reviews: Source | undefined;
  private checks: ChecksState | undefined;
  private timeline: TimelineState;
  private closed = false;

  constructor(
    private readonly host: Connection,
    target: string,
    private readonly update: () => void,
  ) {
    const previews = new Map<string, Preview>();
    const asked = new Set<string>();
    this.timeline = {
      open: new Set(),
      previews,
      links: new Map(),
      wantPreview: (message: ConversationMessage) => {
        if (asked.has(message.body) || !this.reference) return;
        asked.add(message.body);
        this.host
          .request<Preview>("document/preview", { text: message.body, html: message.bodyHtml, base: this.reference.url })
          .then((preview) => {
            previews.set(message.body, preview);
            if (!this.closed) this.update();
          })
          .catch((error) => log("could not preview a message:", error));
      },
    };
    this.show(target);
  }

  /** Shows `target`, as when the panel opens or is retargeted. */
  show(target: string): void {
    this.stop();
    this.reference = parseGitHubReference(target);
    this.tab = "discussion";
    this.discussion = undefined;
    this.reviews = undefined;
    this.checks = undefined;
    this.timeline.open.clear();
    void this.load();
  }

  private async load(): Promise<void> {
    const reference = this.reference;
    if (!reference) return;
    const controller = new AbortController();
    this.controller = controller;
    this.result = undefined;
    this.update();
    try {
      const details = await loadGitHubDetails(reference, AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]));
      if (controller.signal.aborted) return;
      this.result = details;
      if (reference.kind === "pull") {
        this.discussion = new Source(reference.url, "discussion", this.update);
        this.reviews = new Source(reference.url, "reviews", this.update);
      }
    } catch (error) {
      if (controller.signal.aborted) return;
      this.result = error instanceof Error ? error.message : String(error);
    }
    this.update();
  }

  private async loadChecks(): Promise<void> {
    const reference = this.reference;
    const details = this.result;
    if (!reference || typeof details !== "object") return;
    const state = (this.checks ??= {
      result: details.headSha ? loading : unavailableChecks,
      attempt: 0,
      closed: new Set(["skipped"]),
      links: new Map(),
    });
    if (!details.headSha) return;
    state.result = loading;
    this.update();
    const summary = await loadGitHubChecks(reference.repository, details.headSha, AbortSignal.timeout(10_000));
    if (this.checks !== state || this.closed) return;
    state.result = summary;
    this.update();
  }

  private stop(): void {
    this.controller.abort();
    this.discussion?.stop();
    this.reviews?.stop();
  }

  close(): void {
    this.closed = true;
    this.stop();
  }

  private async copy(url: string, title: string): Promise<void> {
    try {
      await this.host.request("clipboard/write", { text: url });
      await this.host.request("ui/toast", { title, tone: "success" });
    } catch {
      await this.host
        .request("ui/toast", { title: "Couldn’t copy the link. Try again from the actions.", tone: "error" })
        .catch(() => {});
    }
  }

  event(id: string, event: string, value: unknown): unknown {
    const reference = this.reference;
    if (id === "retry") {
      this.attempt++;
      return this.load();
    }
    if (id === "open-github" && reference) return this.host.request("ui/openUrl", { url: reference.url }).catch(() => {});
    if (id === "tabs" && (value === "discussion" || value === "checks")) {
      this.tab = value;
      if (value === "checks" && !this.checks) return this.loadChecks();
      return;
    }
    if (id === "checks-retry" && this.checks) {
      this.checks.attempt++;
      return this.loadChecks();
    }
    const [kind, key] = [id.slice(0, id.indexOf(":")), id.slice(id.indexOf(":") + 1)];
    switch (kind) {
      case "message":
      case "run": {
        const open = kind === "run" ? `run:${key}` : key;
        if (event === "toggle" || event === "click") {
          if (!this.timeline.open.delete(open)) this.timeline.open.add(open);
        }
        return;
      }
      case "copy": {
        const url = this.timeline.links.get(key);
        if (url) return this.copy(url, "Comment link copied");
        return;
      }
      case "check": {
        const url = this.checks?.links.get(key);
        if (url) return this.copy(url, "Link copied");
        return;
      }
      case "group":
        if (this.checks && !this.checks.closed.delete(key)) this.checks.closed.add(key);
        return;
      case "more":
      case "retry": {
        const source = key === "discussion" ? this.discussion : key === "reviews" ? this.reviews : undefined;
        if (kind === "more") source?.loadMore();
        else source?.retry();
        return;
      }
    }
  }

  private identity(reference: GitHubReference): Element {
    const [owner, name] = reference.repository.split("/");
    return el.row(
      [
        el.icon(icons[reference.kind], { size: 22 }),
        reference.kind === "pull"
          ? el.column(
              [
                el.row(
                  [
                    el.link(owner ?? "", `https://github.com/${owner}`, { external: true }),
                    el.text("/", { tone: "subtle" }),
                    el.link(name ?? "", `https://github.com/${reference.repository}`, { external: true }),
                  ],
                  { gap: "sm" },
                ),
                el.text(labels.pull, { style: "small", tone: "subtle" }),
              ],
              { gap: "none", grow: true },
            )
          : el.column(
              [
                el.text(reference.repository, { style: "small", tone: "subtle" }),
                el.text(`${labels[reference.kind]} ${reference.label}`, { style: "label" }),
              ],
              { gap: "none", grow: true },
            ),
      ],
      { gap: "sm" },
    );
  }

  private title(reference: GitHubReference): Element | false {
    const details = typeof this.result === "object" ? this.result : undefined;
    if (reference.kind !== "pull") return !!details?.title && el.text(details.title, { style: "large" });
    return el.link(`${details?.title ?? labels.pull} ${reference.label}`, reference.url, { external: true });
  }

  private byline(reference: GitHubReference, details: GitHubDetails): Element {
    const state = details.state.toLowerCase();
    const pull = reference.kind === "pull";
    const icon = pull ? stateIcons[state] : undefined;
    return el.row(
      [
        details.state &&
          el.row(
            [
              icon && el.icon(icon, { size: 12 }),
              el.badge(capitalize(details.state), { tone: pull ? (stateTones[state as keyof typeof stateTones] ?? "default") : "default" }),
            ],
            { gap: "sm" },
          ),
        details.author &&
          el.row(
            [
              el.text("by", { style: "small", tone: "subtle" }),
              details.authorUrl ? el.link(details.author, details.authorUrl, { external: true }) : el.text(details.author, { style: "small" }),
            ],
            { gap: "sm" },
          ),
      ],
      { gap: "sm", wrap: true },
    );
  }

  private facts(reference: GitHubReference, details: GitHubDetails): Element | false {
    if (!details.facts.length && reference.kind !== "pull") return false;
    const row = (label: string, value: Element) =>
      el.row([el.text(label, { style: "small", tone: "subtle" }), value], { justify: "between", gap: "md" });
    const branch = (branch: { label: string; url?: string | undefined }) =>
      branch.url ? el.link(branch.label, branch.url, { external: true }) : el.text(branch.label, { style: "code" });
    const rows = details.facts.map(([label, value]) =>
      row(
        label,
        typeof value !== "object"
          ? el.text(String(value), { style: "small" })
          : "head" in value
            ? el.row([branch(value.head), el.text("→", { style: "small" }), branch(value.base)], { gap: "sm", wrap: true })
            : el.row(
                [
                  el.text(`+${value.additions}`, { style: "small", tone: "success" }),
                  el.text("/", { style: "small" }),
                  el.text(`−${value.deletions}`, { style: "small", tone: "danger" }),
                ],
                { gap: "sm" },
              ),
      ),
    );
    if (reference.kind === "pull")
      rows.push(
        row(
          "Last updated",
          details.updatedAt
            ? el.text(relative(details.updatedAt), { style: "small", tooltip: local(details.updatedAt) })
            : el.text("Unavailable", { style: "small" }),
        ),
      );
    return el.card(rows, { padding: "sm", gap: "sm" });
  }

  private pullContent(reference: GitHubReference, details: GitHubDetails): Element {
    return el.column(
      [
        el.tabs(
          "tabs",
          [
            { value: "discussion", label: "Discussion" },
            { value: "checks", label: "Checks" },
          ],
          this.tab,
        ),
        this.tab === "discussion" && this.discussion && this.reviews
          ? timeline(this.timeline, details, reference.url, this.discussion, this.reviews)
          : this.tab === "checks" && this.checks
            ? checksView(this.checks, !!details.headSha)
            : false,
      ],
      { gap: "md" },
    );
  }

  render(): Element {
    const reference = this.reference;
    if (!reference) return el.text("Unsupported GitHub link.", { tone: "subtle" });
    const result = this.result;
    const body: (Element | false | undefined)[] = [
      this.identity(reference),
      reference.kind !== "pull" && el.button("open-github", "Open on GitHub", { icon: "ArrowSquareOut", fill: true }),
      reference.kind === "pull" && typeof result !== "object" && this.title(reference),
    ];
    if (result === undefined) body.push(el.text("Loading from GitHub…", { tone: "subtle" }));
    else if (typeof result === "string")
      body.push(
        el.column(
          [
            el.text(result),
            el.row(
              [el.button("open-github", "Open on Github", { variant: "prominent" }), el.button("retry", "Retry")],
              { gap: "sm" },
            ),
          ],
          { gap: "sm" },
        ),
      );
    else {
      if (reference.kind === "pull") body.push(this.title(reference));
      body.push(this.byline(reference, result));
      if (reference.kind !== "pull") body.push(this.title(reference));
      body.push(this.facts(reference, result));
      if (reference.kind === "pull") body.push(this.pullContent(reference, result));
      else if (result.body) body.push(el.document(result.body, { html: result.bodyHtml, base: reference.url }));
    }
    body.push(el.text("Public GitHub data · Open on GitHub for private repositories and actions.", { style: "small", tone: "subtle" }));
    return el.column(body.filter((child): child is Element => !!child), { gap: "md" });
  }
}
