// Diffs (`bundled/diffs`): a code diff message (kind 40008) shows as a
// card with a preview of its patch, and expands to a dialog with a
// unified or split view.

import { log, type Params, type PluginSpec } from "@buzz/plugin-sdk";
import { type FileData, parsePatch } from "./parse.ts";
import { card, type DiffMeta, expanded, header, type Layout } from "./view.ts";

interface Seen {
  content: string;
  meta: DiffMeta;
  files: FileData[] | undefined;
}

/** Messages drawn lately, so the dialog has the patch. */
const seen = new Map<string, Seen>();
const KEEP = 200;

function remember(id: string, entry: Seen): void {
  seen.delete(id);
  seen.set(id, entry);
  while (seen.size > KEEP) seen.delete(seen.keys().next().value as string);
}

function read(message: Params): Seen | undefined {
  const meta = message?.diff as DiffMeta | null | undefined;
  if (!meta || typeof message.content !== "string") return undefined;
  const known = seen.get(message.id);
  if (known && known.content === message.content) return known;
  const entry = { content: message.content, meta, files: parsePatch(message.content) };
  remember(message.id, entry);
  return entry;
}

export const diffs: PluginSpec = {
  contributions: {
    messageRenderers: [{ id: "diff", title: "Code diff", kinds: [40008], order: 0 }],
  },
  setup(host) {
    host.handle("message/render", (params: Params) => {
      const entry = read(params.message);
      return entry ? { tree: card(entry.content, entry.meta, entry.files) } : null;
    });
    host.on("render/event", (params: Params) => {
      if (params.renderer !== "diff" || params.id !== "expand" || typeof params.message !== "string") return;
      const entry = seen.get(params.message);
      if (!entry) return;
      host
        .request("ui/openDialog", {
          id: "expanded",
          title: header(entry.meta).title,
          size: "expanded",
          context: { message: params.message },
        })
        .catch((error) => log("could not open the diff:", error));
    });
  },
  views: {
    expanded: (open) => {
      const id = (open.context as { message?: string } | undefined)?.message;
      let layout: Layout = "unified";
      return {
        render: () => {
          const entry = id ? seen.get(id) : undefined;
          if (!entry) return { type: "text", text: "This diff is no longer available.", tone: "subtle" };
          return expanded(entry.content, entry.meta, entry.files, layout);
        },
        event: (event) => {
          if (event.id === "unified" || event.id === "split") layout = event.id;
        },
      };
    },
  },
};
