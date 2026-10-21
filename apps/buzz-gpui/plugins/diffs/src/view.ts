// What a diff message shows (buzz-app `DiffMessage.tsx`, `DiffViewer.tsx`):
// a card with the file, its source and a preview of the patch, and the
// expanded dialog's unified or split view.

import { type CodeCell, type CodeTone, type Element, el } from "@buzz/plugin-sdk";
import { type Change, diffType, type FileData, fileLabel, labels } from "./parse.ts";

/** A diff message's details (`ChannelMessage.diff`). */
export interface DiffMeta {
  filePath?: string | null;
  repoUrl?: string | null;
  commitSha?: string | null;
  description?: string | null;
  truncated?: boolean;
}

export type Layout = "unified" | "split";

/** Lines the in-message preview shows; the dialog shows the rest. */
export const PREVIEW_LINES = 200;

/** `safeMessageUrl`: credential-free HTTPS, as the URL parser writes it. */
export function safeMessageUrl(value: string): string | undefined {
  try {
    const url = new URL(value);
    return url.protocol === "https:" && !url.username && !url.password ? url.href : undefined;
  } catch {
    return undefined;
  }
}

/** The card's title, its source link and the commit's short label. */
export function header(meta: DiffMeta): { title: string; source?: string; commitLabel?: string } {
  const repoUrl = meta.repoUrl ? safeMessageUrl(meta.repoUrl) : undefined;
  const commit = meta.commitSha ?? undefined;
  const commitUrl =
    repoUrl && commit && /^[0-9a-f]{7,64}$/i.test(commit) ? `${repoUrl.replace(/\/$/, "")}/commit/${commit}` : undefined;
  const source = commitUrl || repoUrl;
  const short = commit?.slice(0, 7);
  return {
    title: meta.filePath || "Diff",
    source,
    commitLabel: source ? short || "Source" : short || undefined,
  };
}

/** Tabs to 8-column stops, as a browser's `pre-wrap` keeps them. */
export function expandTabs(text: string): string {
  if (!text.includes("\t")) return text;
  let out = "";
  let column = 0;
  for (const char of text) {
    if (char === "\t") {
      const width = 8 - (column % 8);
      out += " ".repeat(width);
      column += width;
    } else if (char === "\n") {
      out += char;
      column = 0;
    } else {
      out += char;
      column++;
    }
  }
  return out;
}

const tone = (change: Change): CodeTone =>
  change.type === "insert" ? "added" : change.type === "delete" ? "removed" : "context";

const oldLine = (change: Change) =>
  change.type === "normal" ? change.oldLineNumber : change.type === "delete" ? change.lineNumber : undefined;
const newLine = (change: Change) =>
  change.type === "normal" ? change.newLineNumber : change.type === "insert" ? change.lineNumber : undefined;

const number = (value: number | undefined, change: Change): CodeCell => ({
  kind: "number",
  text: value?.toString() ?? "",
  tone: tone(change),
});
const code = (change: Change): CodeCell => ({ text: expandTabs(change.content), tone: tone(change) });
const omitted: CodeCell[] = [{ kind: "omitted" }, { text: "" }];

/** The split view's rows: a delete followed by an insert share a row (react-diff-view's `SplitHunk`). */
export function splitRows(changes: Change[]): [Change | undefined, Change | undefined][] {
  const rows: [Change | undefined, Change | undefined][] = [];
  for (let i = 0; i < changes.length; i++) {
    const change = changes[i] as Change;
    if (change.type === "normal") rows.push([change, change]);
    else if (change.type === "delete") {
      const next = changes[i + 1];
      if (next?.type === "insert") {
        rows.push([change, next]);
        i++;
      } else rows.push([change, undefined]);
    } else rows.push([undefined, change]);
  }
  return rows;
}

function unified(file: FileData, limit: number): Element {
  const lines = file.hunks
    .flatMap((hunk) => hunk.changes)
    .slice(0, limit)
    .map((change) => ({ cells: [number(oldLine(change), change), number(newLine(change), change), code(change)] }));
  return el.code(lines);
}

/** Old and new side by side. A new or deleted file has one side only. */
function split(file: FileData): Element {
  const monotonous = file.type === "add" || file.type === "delete";
  const oldSide = (change?: Change) => (change ? [number(oldLine(change), change), code(change)] : omitted);
  const newSide = (change?: Change) => (change ? [number(newLine(change), change), code(change)] : omitted);
  const lines = file.hunks.flatMap((hunk) =>
    splitRows(hunk.changes).map(([old, next]) => ({
      cells: monotonous ? (old ? oldSide(old) : newSide(next)) : [...oldSide(old), ...newSide(next)],
    })),
  );
  // Narrower than 48rem, it scrolls sideways.
  return el.code(lines, { minWidth: 768 });
}

const note = (text: string) => el.column([el.text(text, { style: "small", tone: "subtle" })], { padding: "sm" });

/** `DiffViewer`: the raw fallback, the empty state, or each file. With a `limit`, only that many lines show. */
export function viewer(
  content: string,
  files: FileData[] | undefined,
  filePath: string | undefined,
  layout: Layout,
  limit = Number.POSITIVE_INFINITY,
): Element {
  if (!files) {
    // A `pre` does not show a final newline as an empty line.
    const text = content.endsWith("\n") ? content.slice(0, -1) : content;
    return el.code(
      expandTabs(text)
        .split("\n")
        .map((line) => ({ text: line })),
      { wrap: false },
    );
  }
  if (!files.length) return el.text("No diff content", { style: "small" });
  const lines = (file: FileData) => file.hunks.reduce((sum, hunk) => sum + hunk.changes.length, 0);
  let left = limit;
  const shown = files.map((file) => {
    const count = Math.min(left, lines(file));
    left -= count;
    return count;
  });
  const total = files.reduce((sum, file) => sum + lines(file), 0);
  const hidden = total - shown.reduce((sum, count) => sum + count, 0);
  const children: Element[] = [];
  for (const [index, file] of files.entries()) {
    const count = shown[index] ?? 0;
    // Files past the limit are left out; one without lines still shows.
    if (count === 0 && lines(file) > 0) break;
    const label = fileLabel(file, filePath);
    const type = diffType(file);
    const changes = file.hunks.flatMap((hunk) => hunk.changes);
    const additions = changes.filter((change) => change.type === "insert").length;
    const deletions = changes.filter((change) => change.type === "delete").length;
    const showHeader = files.length > 1 || label !== filePath || type !== "modify";
    children.push(
      el.column(
        [
          showHeader &&
            el.row(
              [
                el.text(label, { style: "code", truncate: true }),
                el.text(labels[type], { style: "code" }),
                el.text(`+${additions}`, { style: "code", tone: "success" }),
                el.text(`−${deletions}`, { style: "code", tone: "danger" }),
              ],
              { background: "inset", padding: "sm" },
            ),
          !file.hunks.length
            ? note("No textual hunks in this diff.")
            : layout === "split"
              ? split(file)
              : unified(file, count),
        ],
        { gap: "none" },
      ),
    );
  }
  if (hidden > 0) children.push(note(`${hidden} more ${hidden === 1 ? "line" : "lines"}. Expand the diff to see them all.`));
  return el.column(children, { gap: "md" });
}

/** "Diff truncated." with the way to the full patch. */
export function truncated(source: string | undefined): Element {
  return source
    ? el.column(
        [
          el.row([el.text("Diff truncated.", { style: "small", tone: "subtle" }), el.link("View full diff at the source repository", source)], {
            wrap: true,
          }),
        ],
        { padding: "sm" },
      )
    : note("Diff truncated. View the full diff at the source repository.");
}

/** The card in the conversation. */
export function card(content: string, meta: DiffMeta, files: FileData[] | undefined): Element {
  const { title, source, commitLabel } = header(meta);
  return el.card(
    [
      el.row(
        [
          el.text(title, { style: "code", truncate: true }),
          el.row(
            [
              commitLabel && (source ? el.link(commitLabel, source) : el.text(commitLabel, { style: "code" })),
              el.button("expand", "", { icon: "ArrowsOut", size: "sm", variant: "ghost" }),
            ],
            { gap: "sm" },
          ),
        ],
        { justify: "between", gap: "sm", background: "inset", padding: "sm" },
      ),
      meta.description && note(meta.description),
      el.scroll([viewer(content, files, meta.filePath ?? undefined, "unified", PREVIEW_LINES)], {
        maxHeight: 400,
        padding: "md",
      }),
      meta.truncated && truncated(source),
    ],
    { padding: "none", gap: "none" },
  );
}

/** The expanded dialog's body: the layout toggle, the notice and every line. */
export function expanded(content: string, meta: DiffMeta, files: FileData[] | undefined, layout: Layout): Element {
  const { source } = header(meta);
  const toggle = (id: Layout, label: string) =>
    el.button(id, label, { size: "sm", variant: layout === id ? "subtle" : "ghost" });
  return el.column(
    [
      el.row([toggle("unified", "Unified"), toggle("split", "Split")], { gap: "sm" }),
      meta.truncated && truncated(source),
      viewer(content, files, meta.filePath ?? undefined, layout),
    ],
    { gap: "md" },
  );
}
