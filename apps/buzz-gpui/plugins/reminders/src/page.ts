// The Reminders page (buzz-app `RemindersPage.tsx`): pending reminders by
// Overdue, Today and Upcoming, then Done, each with Open, Snooze, Done and
// Cancel.

import { type Connection, type Element, el } from "@buzz/plugin-sdk";
import type { Hub } from "./hub.ts";
import { endOfToday, formatDue, groupReminders, navigableTarget, TIME_PRESETS } from "./model.ts";
import type { Reminder } from "./types.ts";

const notice = (text: string) => el.text(text, { tone: "subtle" });

export class RemindersPage {
  private error: string | undefined;
  private retrying = false;
  private showDone = false;

  constructor(
    private readonly host: Connection,
    private readonly hub: Hub,
    private readonly update: () => void,
  ) {}

  private run(action: Promise<unknown>): void {
    this.error = undefined;
    action.catch(() => {
      this.error = "The reminder could not be updated. Try again.";
      this.update();
    });
  }

  private find(id: string): Reminder | undefined {
    return this.hub.state.reminders.find((reminder) => reminder.id === id);
  }

  async event(id: string, value: unknown): Promise<void> {
    if (id === "retry") {
      this.retrying = true;
      this.update();
      await this.host.request("community/refresh", { what: "reminders" }).catch(() => {});
      this.retrying = false;
      return;
    }
    if (id === "done-group") {
      this.showDone = !this.showDone;
      return;
    }
    const match = /^(open|snooze|done|cancel):(.+)$/.exec(id);
    const reminder = match?.[2] ? this.find(match[2]) : undefined;
    if (!match || !reminder) return;
    switch (match[1]) {
      case "open": {
        const target = navigableTarget(reminder);
        if (!target) return;
        const result = await this.host
          .request<{ status: string }>("ui/navigate", {
            to: "conversation",
            channel: target.channelId,
            message: target.eventId,
          })
          .catch(() => ({ status: "failed" }));
        if (result.status === "failed") this.error = "This message could not be opened.";
        return;
      }
      case "snooze": {
        const preset = TIME_PRESETS.find((preset) => preset.label === value);
        if (preset) this.run(this.host.request("reminders/snooze", { id: reminder.id, notBefore: preset.at() }));
        return;
      }
      case "done":
        return this.run(this.host.request("reminders/complete", { id: reminder.id }));
      case "cancel":
        return this.run(this.host.request("reminders/cancel", { id: reminder.id }));
    }
  }

  private row(reminder: Reminder): Element {
    const pending = reminder.status === "pending";
    return el.row(
      [
        el.column(
          [
            el.text(reminder.target?.preview || reminder.note || ""),
            reminder.target && reminder.note && el.text(reminder.note, { tone: "subtle" }),
            reminder.notBefore !== undefined && el.text(formatDue(reminder.notBefore), { style: "label", tone: "subtle" }),
          ],
          { gap: "none", grow: true },
        ),
        el.row(
          [
            navigableTarget(reminder) && this.hub.community?.status === "ready" && el.button(`open:${reminder.id}`, "Open"),
            pending &&
              el.menu(
                `snooze:${reminder.id}`,
                TIME_PRESETS.map((preset) => ({ value: preset.label, label: preset.label })),
                { label: "Snooze" },
              ),
            pending && el.button(`done:${reminder.id}`, "Done"),
            pending && el.button(`cancel:${reminder.id}`, "Cancel", { variant: "ghost" }),
          ],
          { gap: "sm" },
        ),
      ],
      { gap: "md" },
    );
  }

  render(): Element {
    const { community, state, now } = this.hub;
    const ready = community?.status === "ready";
    const groups = groupReminders(state.reminders, now, endOfToday());
    const done = groups.find((group) => group.label === "Done");
    const active = groups.filter((group) => group !== done);
    const status = !ready
      ? community?.status === "error"
        ? (community.error ?? "Could not connect.")
        : "Connect to a community to see your reminders."
      : state.status === "loading" && !state.hydrated
        ? "Loading reminders…"
        : undefined;
    // Any failed read keeps Retry up beside whatever list we have until a
    // read succeeds; live arrivals never clear it.
    const failed = ready && state.error !== undefined && state.status !== "loading";
    const showRetry = failed || this.retrying;
    const list = (items: readonly Reminder[]) => el.column(items.map((reminder) => this.row(reminder)), { gap: "md" });
    const body: Element[] = [];
    if (status) body.push(notice(status));
    else if (!(failed && !state.hydrated && !state.reminders.length)) {
      if (active.length)
        for (const group of active)
          body.push(el.column([el.text(group.label, { style: "label", tone: "subtle" }), list(group.reminders)], { gap: "sm" }));
      else
        body.push(
          el.column(
            [
              el.text("Active", { style: "label", tone: "subtle" }),
              notice("No active reminders"),
              !done && notice("Use Remind me in a message's menu to get reminded later."),
            ],
            { gap: "sm" },
          ),
        );
      if (done)
        body.push(
          el.column(
            [
              el.button("done-group", `${this.showDone ? "▾" : "▸"} Done · ${done.reminders.length}`, {
                variant: "ghost",
                size: "sm",
              }),
              this.showDone && list(done.reminders),
            ],
            { gap: "sm" },
          ),
        );
    }
    return el.column(
      [
        el.header("Reminders", { icon: "Alarm" }),
        el.column(
          [
            this.error && el.text(this.error, { tone: "danger" }),
            showRetry &&
              el.row([notice("Some reminders could not be loaded."), el.button("retry", "Retry reminders", { loading: this.retrying })], {
                gap: "sm",
              }),
            ...body,
          ],
          { gap: "lg", padding: "lg" },
        ),
      ],
      { grow: true, gap: "none" },
    );
  }
}
