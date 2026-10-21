import { expect, it } from "vitest";
import { diffType, fileLabel, parsePatch } from "./parse.ts";

const patch =
  "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-old\n+new\n";
it("parses multiple files and preserves change line numbers", () => {
  const files = parsePatch(patch + patch.replaceAll("a.ts", "b.ts"));
  expect(files).toHaveLength(2);
  expect(files?.[0]?.hunks[0]?.changes).toMatchObject([
    { type: "delete", content: "old", lineNumber: 1 },
    { type: "insert", content: "new", lineNumber: 1 },
  ]);
  expect(files?.[1] && fileLabel(files[1])).toBe("b.ts");
});
it.each([
  [
    "new file",
    "/dev/null",
    "b/a.ts",
    "-0,0 +1,2",
    "+one\n+two",
    0,
    2,
    "insert",
    1,
  ],
  [
    "deleted file",
    "a/a.ts",
    "/dev/null",
    "-1,2 +0,0",
    "-one\n-two",
    2,
    0,
    "delete",
    1,
  ],
  [
    "zero-context insertion",
    "a/a.ts",
    "b/a.ts",
    "-3,0 +4,2",
    "+one\n+two",
    0,
    2,
    "insert",
    4,
  ],
  [
    "zero-context deletion",
    "a/a.ts",
    "b/a.ts",
    "-4,2 +3,0",
    "-one\n-two",
    2,
    0,
    "delete",
    4,
  ],
])(
  "preserves explicit zero counts for %s",
  (_name, oldPath, newPath, range, lines, oldLines, newLines, type, lineNumber) => {
    const files = parsePatch(
      `diff --git a/a.ts b/a.ts\n--- ${oldPath}\n+++ ${newPath}\n@@ ${range} @@\n${lines}\n`,
    );
    expect(files).toHaveLength(1);
    expect(files?.[0]?.hunks[0]).toMatchObject({
      oldLines,
      newLines,
      changes: [
        { type, content: "one", lineNumber },
        { type, content: "two", lineNumber: lineNumber + 1 },
      ],
    });
  },
);
it.each([
  [
    "rename",
    "diff --git a/old.ts b/new.ts\nsimilarity index 100%\nrename from old.ts\nrename to new.ts\n",
    "old.ts → new.ts",
    "rename",
  ],
  [
    "binary",
    "diff --git a/image.png b/image.png\nindex 1234567..abcdef0 100644\nBinary files a/image.png and b/image.png differ\n",
    "image.png",
    "modify",
  ],
  [
    "mode",
    "diff --git a/run b/run\nold mode 100644\nnew mode 100755\n",
    "run",
    "modify",
  ],
])(
  "retains %s changes with no textual hunks",
  (_name, content, label, type) => {
    const files = parsePatch(content);
    expect(files).toHaveLength(1);
    const file = files?.[0];
    if (!file) throw new Error("Missing file");
    expect(file.hunks).toEqual([]);
    expect(fileLabel(file)).toBe(label);
    expect(diffType(file)).toBe(type);
  },
);
it.each([
  "malformed <script>alert(1)</script>",
  patch.replace("-1 +1", "-1,3 +1,3"),
  `${patch}UNPARSED_TRAILER\n`,
  patch.replace("-old", "UNKNOWN_HUNK_LINE\n-old"),
  patch.replace("--- a/a.ts", "UNKNOWN_HEADER\n--- a/a.ts"),
  `PREAMBLE\n${patch}`,
  `${patch}\\ not a newline marker\n`,
  `${patch}\\ No newline at end of file\n\\ No newline at end of file\n`,
  "diff --git a/a.bin b/a.bin\nGIT binary patch\nliteral 3\nKcmZQzWC8#H2mk;8\n",
  "x".repeat(100_001),
])(
  "falls back without discarding malformed, incomplete or excessive content",
  (content) => {
    expect(parsePatch(content)).toBeUndefined();
  },
);
it("distinguishes empty patches and supports plain unified patches", () => {
  expect(parsePatch(" \n")).toEqual([]);
  expect(
    parsePatch(patch.slice(patch.indexOf("---")))?.[0]?.hunks,
  ).toHaveLength(1);
});

it("retains multiple hunks and exact no-newline markers", () => {
  const files = parsePatch(
    patch.replace("-old\n", "-old\n\\ No newline at end of file\n") +
      "\\ No newline at end of file\n@@ -4 +4 @@\n-before\n+after\n",
  );
  expect(files?.[0]?.hunks).toHaveLength(2);
  expect(files?.[0]).toMatchObject({
    oldEndingNewLine: false,
    newEndingNewLine: false,
  });
  expect(files?.[0]?.hunks[1]?.changes.map((change) => change.content)).toEqual(
    ["before", "after"],
  );
});
