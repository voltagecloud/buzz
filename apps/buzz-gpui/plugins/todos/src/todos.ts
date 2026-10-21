// Todos (`bundled/todos`): a side panel beside each channel listing the
// todos in its Canvas. Off by default; Settings → Plugins turns it on.

import type { Params, PluginSpec } from "@buzz/plugin-sdk";
import { TodosPanel } from "./panel.ts";

const panels = new Map<string, TodosPanel>();

export const todos: PluginSpec = {
  contributions: {
    panels: [
      {
        id: "todos",
        title: "Todos",
        icon: "ListChecks",
        matches: [],
        launcher: { label: "Todos", placement: "side" },
        layout: "workspace",
      },
    ],
  },
  setup(host) {
    host.on("session/changed", (params: Params) => {
      if (params.what !== "channelList") return;
      for (const panel of panels.values()) if (panel.ownsWatch(params.watch)) panel.channelList(params.value);
    });
    host.on("names/changed", (params: Params) => {
      for (const panel of panels.values()) if (panel.ownsWatch(params.watch)) panel.namesChanged(params.names ?? {});
    });
  },
  views: {
    todos: (open, view) => {
      const panel = new TodosPanel(view.host, view.id, open, view.update);
      panels.set(view.id, panel);
      return {
        render: () => panel.render(),
        event: (event) => panel.event(event.id, event.event, event.value),
        close: () => {
          panels.delete(view.id);
          panel.close();
        },
      };
    },
  },
};
