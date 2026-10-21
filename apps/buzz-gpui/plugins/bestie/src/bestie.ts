// Bestie (`bundled/bestie`): the companion's home page until agent chat
// connects. Off by default; Settings → Plugins turns it on.

import { el, type PluginSpec } from "@buzz/plugin-sdk";

export const bestie: PluginSpec = {
  contributions: {
    pages: [{ id: "bestie", title: "Bestie", icon: "Bestie", layout: "workspace" }],
  },
  views: {
    bestie: () => ({
      render: () =>
        el.column(
          [
            el.header("Bestie", { icon: "Bestie" }),
            el.column(
              [
                el.icon("Bestie", { size: 80 }),
                el.text("Meet your Bestie", { style: "heading" }),
                el.text("Your companion’s home in Buzz. Agent chat isn’t connected yet.", {
                  style: "small",
                  tone: "subtle",
                }),
              ],
              { grow: true, align: "center", justify: "center", gap: "md", padding: "lg" },
            ),
          ],
          { grow: true, gap: "none" },
        ),
    }),
  },
};
