// Reminders (`bundled/reminders`): the Reminders page with its due badge,
// the "Remind me" message action with its alarm marker, and a notification
// when a reminder comes due.

import { log, type Params, type PluginSpec } from "@buzz/plugin-sdk";
import { type Target, RemindDialog } from "./dialog.ts";
import { Hub } from "./hub.ts";
import { RemindersPage } from "./page.ts";

let hub: Hub | undefined;

export const reminders: PluginSpec = {
  contributions: {
    pages: [{ id: "reminders", title: "Reminders", icon: "Alarm", layout: "workspace" }],
    notifications: [{ id: "reminders", label: "Reminders" }],
    messageActions: [{ id: "remind-me", title: "Remind me", icon: "Alarm" }],
  },
  setup(host) {
    const current = (hub = new Hub(host));
    host.on("community/changed", (params: Params) => current.bind(params));
    host.on("reminders/changed", (params: Params) => current.reminders(params));
    host.on("session/changed", (params: Params) => {
      if (params.what === "channelList") current.channelList(params);
    });
    host.on("message/action", async (params: Params) => {
      if (params.action !== "remind-me" || !params.message?.id) return;
      const message = params.message;
      const text = await host
        .request<string>("text/plain", { text: String(message.content ?? "") })
        .catch(() => "");
      const target: Target = {
        message: message.id,
        channel: message.channel,
        preview: text || "New message",
        author: message.author,
      };
      await host
        .request("ui/openDialog", { id: "remind", title: "Remind me", context: target })
        .catch((error) => log("could not open Remind me:", error));
    });
    // The community now; later changes come as `community/changed`.
    void host
      .request<Params>("community/current")
      .then((community) => current.bind(community ?? {}))
      .catch((error) => log("could not read the community:", error));
  },
  views: {
    reminders: (_, view) => {
      if (!hub) throw new Error("not set up");
      const page = new RemindersPage(view.host, hub, view.update);
      const stop = hub.subscribe(view.update);
      return {
        render: () => page.render(),
        event: (event) => page.event(event.id, event.value),
        close: stop,
      };
    },
    remind: (open, view) => {
      const dialog = new RemindDialog(view.host, view.id, open.context as Target | undefined, view.update);
      return {
        render: () => dialog.render(),
        event: (event) => dialog.event(event.id, event.event, event.value),
      };
    },
  },
};
