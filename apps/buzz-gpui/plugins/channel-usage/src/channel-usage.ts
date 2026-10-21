// Channel Usage (`bundled/channel-usage`): "View channel usage" in a
// channel's menu opens a tab of what agents reported using there.

import type { Params, PluginSpec } from "@buzz/plugin-sdk";
import { ChannelUsage } from "./view.ts";

const views = new Map<string, ChannelUsage>();

export const channelUsage: PluginSpec = {
  contributions: {
    panels: [
      {
        id: "usage",
        title: "Usage",
        icon: "Browser",
        matches: [],
        // `eligible`: a member of a live, writable, unarchived channel,
        // while this device saves agent activity.
        menu: { label: "View channel usage", member: true, writable: true, archive: true },
      },
    ],
  },
  setup(host) {
    host.on("community/changed", (params: Params) => {
      for (const view of views.values()) view.community(params);
    });
    host.on("session/changed", (params: Params) => {
      if (params.what !== "channelList") return;
      for (const view of views.values()) if (view.ownsWatch(params.watch)) view.channelList(params.value);
    });
    host.on("names/changed", (params: Params) => {
      for (const view of views.values()) if (view.ownsWatch(params.watch)) view.namesChanged(params.names ?? {});
    });
  },
  views: {
    usage: (open, view) => {
      const usage = new ChannelUsage(view.host, open, view.update);
      views.set(view.id, usage);
      return {
        render: () => usage.render(),
        event: (event) => usage.event(event.id, event.value),
        close: () => {
          views.delete(view.id);
          usage.close();
        },
      };
    },
  },
};
