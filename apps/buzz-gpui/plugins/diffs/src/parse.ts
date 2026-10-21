// Reading a diff message's patch (buzz-app `bundled/diffs/parse.ts`).
//
// buzz-app parses with react-diff-view's `parseDiff`, which wraps
// gitdiff-parser 0.3.1; `parseDiff` is copied here (MIT, react-diff-view
// 3.3.2, `src/utils/parse.ts`) so the React package is not needed. The
// strict checks after it are buzz-app's: a patch that is cut off, binary,
// in an unknown format or over 100k stays readable as raw text instead.

import parser, { type File as FileData } from "gitdiff-parser";

export type { FileData };
export type { Change, Hunk } from "gitdiff-parser";
export type DiffType = "add" | "copy" | "delete" | "modify" | "rename";

function normalizeDiffText(text: string): string {
  // Git diff header:
  //
  // diff --git a/test/fixture/test/ci.go b/test/fixture/test/ci.go
  // index 6829b8a2..4c565f1b 100644
  // --- a/test/fixture/test/ci.go
  // +++ b/test/fixture/test/ci.go
  if (text.startsWith("diff --git")) return text;
  // Unidiff header:
  //
  // --- /test/fixture/test/ci.go 2002-02-21 23:30:39.942229878 -0800
  // +++ /test/fixture/test/ci.go 2002-02-21 23:30:50.442260588 -0800
  const indexOfFirstLineBreak = text.indexOf("\n");
  const indexOfSecondLineBreak = text.indexOf("\n", indexOfFirstLineBreak + 1);
  const firstLine = text.slice(0, indexOfFirstLineBreak);
  const secondLine = text.slice(indexOfFirstLineBreak + 1, indexOfSecondLineBreak);
  const oldPath = firstLine.split(" ").slice(1, -3).join(" ");
  const newPath = secondLine.split(" ").slice(1, -3).join(" ");
  return [
    `diff --git a/${oldPath} b/${newPath}`,
    "index 1111111..2222222 100644",
    `--- a/${oldPath}`,
    `+++ b/${newPath}`,
    text.slice(indexOfSecondLineBreak + 1),
  ].join("\n");
}

/** react-diff-view's `parseDiff`, without its options. */
export function parseDiff(text: string): FileData[] {
  return parser.parse(normalizeDiffText(text.trimStart())).map((file) => ({
    ...file,
    hunks: file.hunks.map((hunk) => ({ ...hunk, isPlain: false })),
  }));
}

export function parsePatch(content: string): FileData[] | undefined {
  // Legacy send-diff caps UTF-8 payloads at 60 KiB. Unbounded relay content
  // stays readable without allocating a diff table.
  if (content.length > 100_000) return undefined;
  if (!content.trim()) return [];
  try {
    const files = parseDiff(content).filter((file) => file.hunks.length || file.oldPath || file.newPath);
    // The parser accepts incomplete hunks. Do not present a cut-off patch as a
    // complete change; the raw fallback preserves even its final partial line.
    const complete = files.every((file) =>
      file.hunks.every((hunk) => {
        // gitdiff-parser 0.3.1 treats explicit ,0 as the omitted-count default
        // of 1. Restore header counts before validating or rendering the hunk.
        const counts = /^@@\s+-\d+(?:,(\d+))?\s+\+\d+(?:,(\d+))?\s+@@/.exec(hunk.content);
        if (!counts) return false;
        hunk.oldLines = Number(counts[1] ?? 1);
        hunk.newLines = Number(counts[2] ?? 1);
        return (
          hunk.changes.filter((change) => change.type !== "insert").length === hunk.oldLines &&
          hunk.changes.filter((change) => change.type !== "delete").length === hunk.newLines
        );
      }),
    );
    return files.length && complete && consumesPatch(content, files) ? files : undefined;
  } catch {
    return undefined;
  }
}

// gitdiff-parser silently skips unknown input. Match each hunk against the
// changes it actually returned; only Git's known file headers may sit outside
// hunks. Unsupported formats (including binary payloads) stay readable as raw.
function consumesPatch(content: string, files: FileData[]): boolean {
  const hunks = files.flatMap((file) => file.hunks);
  let hunkIndex = 0;
  let changes: string[] | undefined;
  let changeIndex = 0;
  let canMarkNewline = false;
  const lines = content.split("\n");
  if (lines.at(-1) === "") lines.pop();
  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      if (changes && changeIndex !== changes.length) return false;
      changes = undefined;
    } else if (line.startsWith("@@")) {
      if (changes && changeIndex !== changes.length) return false;
      const hunk = hunks[hunkIndex++];
      if (!hunk || line !== hunk.content) return false;
      changes = hunk.changes.map(
        (change) => `${change.type === "insert" ? "+" : change.type === "delete" ? "-" : " "}${change.content}`,
      );
      changeIndex = 0;
    } else if (changes) {
      if (line === "\\ No newline at end of file" && canMarkNewline) {
        canMarkNewline = false;
        continue;
      }
      if (line !== changes[changeIndex++]) return false;
      canMarkNewline = true;
      continue;
    } else if (
      !/^(?:--- |\+\+\+ |index [0-9a-f]+\.\.[0-9a-f]+(?: \d+)?$|(?:old|new|deleted file|new file) mode \d+$|(?:dis)?similarity index \d+%$|(?:rename|copy) (?:from|to) |Binary files .+ differ$)/.test(
        line,
      )
    ) {
      return false;
    }
    canMarkNewline = false;
  }
  return hunkIndex === hunks.length && (!changes || changeIndex === changes.length);
}

export function fileLabel(file: FileData, fallback?: string): string {
  const oldPath = file.oldPath === "/dev/null" ? undefined : file.oldPath;
  const newPath = file.newPath === "/dev/null" ? undefined : file.newPath;
  return oldPath && newPath && oldPath !== newPath ? `${oldPath} → ${newPath}` : newPath || oldPath || fallback || "diff";
}

export function diffType(file: FileData): DiffType {
  return (["add", "copy", "delete", "rename"] as string[]).includes(file.type) ? (file.type as DiffType) : "modify";
}

export const labels: Record<DiffType, string> = {
  add: "New file",
  copy: "Copied",
  delete: "Deleted",
  modify: "Modified",
  rename: "Renamed",
};
