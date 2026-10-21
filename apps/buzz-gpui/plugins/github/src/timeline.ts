// A pull request's Discussion tab (buzz-app `GitHubConversation.tsx`): the
// description, then comments, reviews and the merge in order, with runs of
// older ones folded away. A message too long for one line opens to show
// its body.

import { type Element, el, type Tone } from "@buzz/plugin-sdk";
import {
  type ConversationEvent,
  type ConversationMessage,
  conversationEvents,
  type EventRun,
  groupConversationEvents,
} from "./conversation.ts";
import type { GitHubDetails } from "./data.ts";
import type { Source } from "./source.ts";
import { local, relative } from "./time.ts";

/** What `document/preview` says about a body. */
export interface Preview {
  text: string;
  images: string[];
  textOnly: boolean;
  line: string | null;
}

export interface TimelineState {
  /** Messages and runs shown open, by key. */
  open: Set<string>;
  /** Bodies' previews, by `body` text. */
  previews: Map<string, Preview>;
  /** Comment links by message key, for Copy link. */
  links: Map<string, string>;
  /** Asks for a body's preview, once. */
  wantPreview(message: ConversationMessage): void;
}

const reviewLabels: Record<string, string> = {
  APPROVED: "Approved",
  CHANGES_REQUESTED: "Changes requested",
  COMMENTED: "Review comment",
  DISMISSED: "Review dismissed",
};

type Kind = "description" | "review" | "comment" | "merge";

function markerIcon(kind: Kind, state: string | undefined): { name: string; tone: Tone } {
  if (kind === "merge") return { name: "GitMerge", tone: "accent" };
  if (kind === "review") {
    if (state === "APPROVED") return { name: "CheckCircle", tone: "success" };
    if (state === "CHANGES_REQUESTED") return { name: "XCircle", tone: "danger" };
    if (state === "DISMISSED") return { name: "X", tone: "subtle" };
  }
  return { name: "ChatCircle", tone: "subtle" };
}

function message(
  state: TimelineState,
  options: {
    key: string;
    message: ConversationMessage;
    url: string;
    label: string;
    kind: Kind;
    reviewState?: string | undefined;
    commentUrl?: string | undefined;
    fallback: string;
  },
): Element {
  const { key, message, url, label, kind, commentUrl, fallback } = options;
  if (message.body) state.wantPreview(message);
  const preview = message.body ? state.previews.get(message.body) : undefined;
  const hasBody = !!message.body.trim() || !!fallback;
  // Only a text body with no images can show on one line.
  const canFit = !message.body || (!!preview?.textOnly && !preview.images.length);
  const fit = canFit && hasBody ? (message.body ? (preview?.line ?? undefined) : fallback) : undefined;
  const open = state.open.has(key);
  if (commentUrl) state.links.set(key, commentUrl);
  const icon = markerIcon(kind, options.reviewState);
  const author = message.author || "Unknown author";
  return el.collapsible(
    `message:${key}`,
    hasBody
      ? [message.body ? el.document(message.body, { html: message.bodyHtml, base: url }) : el.text(fallback, { style: "small" })]
      : [],
    {
    open,
    marker: [
      kind === "description"
        ? el.avatar("", { url: message.authorAvatar, name: author })
        : el.icon(icon.name, { size: 20, tone: icon.tone }),
    ],
    markerTooltip: `Toggle ${label}`,
    header: [
      el.row(
        [
          message.authorUrl ? el.link(author, message.authorUrl, { external: true }) : el.text(author, { style: "small" }),
          el.text(label, {
            style: "small",
            tone: "subtle",
            ...(kind === "review" ? { tooltip: "Submitted review event, not the PR’s current approval status" } : {}),
          }),
          el.row(
            [
              message.createdAt
                ? el.text(relative(message.createdAt), { style: "small", tone: "subtle", tooltip: local(message.createdAt) })
                : el.text("Time unavailable", { style: "small", tone: "subtle" }),
            ],
            { grow: true, justify: "end" },
          ),
        ],
        { gap: "sm", wrap: true },
      ),
    ],
    preview: (message.body ? preview?.text : "") || fallback,
    ...(fit ? { fit } : {}),
    expandable: hasBody,
    below: preview?.images.length ? [el.thumbnails(preview.images, { label })] : [],
    actions:
      commentUrl && !open
        ? [el.menu(`copy:${key}`, [{ value: "copy", label: "Copy link", icon: "Link" }], { icon: "DotsThree", tooltip: "Comment actions" })]
        : [],
    footer: commentUrl && open && hasBody ? [el.button(`copy:${key}`, "Copy link", { icon: "Link", size: "sm" })] : [],
    },
  );
}

function eventMessage(state: TimelineState, event: ConversationEvent, url: string): Element {
  const commentUrl = new URL(url);
  commentUrl.hash = `${event.kind === "review" ? "pullrequestreview" : "issuecomment"}-${event.message.id}`;
  return message(state, {
    key: event.key,
    message: event.message,
    url,
    commentUrl: event.kind === "merge" ? undefined : commentUrl.href,
    label:
      event.kind === "merge" ? "Merged" : event.kind === "review" ? (reviewLabels[event.message.state ?? ""] ?? "Reviewed") : "Comment",
    kind: event.kind === "discussion" ? "comment" : event.kind,
    reviewState: event.message.state,
    fallback: event.kind === "discussion" ? "No message provided" : "",
  });
}

function history(state: TimelineState, run: EventRun, url: string): Element {
  // Runs are keyed apart from the messages in them.
  const open = state.open.has(`run:${run.key}`);
  const label = run.events.length === 1 ? "message" : "messages";
  const summary = run.afterMerge ? `${run.events.length} ${label} after merge` : `${run.events.length} earlier ${label}`;
  const hide = run.afterMerge ? "Hide messages after merge" : "Hide earlier messages";
  const authors = [...new Set(run.events.map((event) => event.message.author || "Unknown author"))].join(", ");
  return el.column(
    [
      el.button(`run:${run.key}`, open ? hide : `${summary} · ${authors}`, {
        icon: open ? "CaretUp" : "DotsThree",
        variant: "ghost",
        size: "sm",
      }),
      open && el.column(run.events.map((event) => eventMessage(state, event, url)), { gap: "none" }),
    ],
    { gap: "none" },
  );
}

function sourceStatus(label: string, id: string, source: Source): Element | false {
  if (!source.loading && !source.error && !source.next) return false;
  return el.row(
    [
      el.text(`${label} · ${source.entries.length} loaded`, { style: "small" }),
      source.loading
        ? el.text(`Loading ${label.toLowerCase()}…`, { style: "small", tone: "subtle" })
        : source.error
          ? el.text(source.error, { style: "small", tone: "danger" })
          : false,
      (source.error || source.next) &&
        el.button(`${source.error ? "retry" : "more"}:${id}`, `${source.error ? "Retry" : "Load more"} ${label.toLowerCase()}`, {
          size: "sm",
          loading: source.loading,
        }),
    ],
    { gap: "sm", wrap: true },
  );
}

export function timeline(
  state: TimelineState,
  details: GitHubDetails,
  url: string,
  discussion: Source,
  reviews: Source,
): Element {
  const events = conversationEvents(discussion.entries, reviews.entries, details);
  const incomplete = [discussion, reviews].some((source) => source.loading || source.error || source.next);
  return el.column(
    [
      el.column(
        [
          message(state, {
            key: "description",
            message: details,
            url,
            label: "Description",
            kind: "description",
            fallback: "No description provided",
          }),
          ...groupConversationEvents(events).map((event) =>
            event.kind === "history" ? history(state, event, url) : eventMessage(state, event, url),
          ),
        ],
        { gap: "none" },
      ),
      incomplete &&
        el.column(
          [
            el.text("Loaded conversation only · some sources are incomplete.", { style: "small", tone: "subtle" }),
            sourceStatus("Discussion", "discussion", discussion),
            sourceStatus("Reviews", "reviews", reviews),
          ],
          { gap: "sm" },
        ),
    ],
    { gap: "md" },
  );
}
