// The selected community's reminders and the one due-time timer, which
// also advances the clock the badge and page read (buzz-app
// `bundled/reminders/index.tsx`). A per-community baseline, saved as
// buzz-app's `buzz-reminders-checked.v1`, lets reminders that came due while
// the app was closed notify on the next launch.

import { type Connection, log, type Params, readJson, writeJson } from "@buzz/plugin-sdk";
import { countDue, dueSince, navigableTarget, nextDelay, nowSeconds, pendingMessageIds } from "./model.ts";
import { fromList, type Reminder, type RemindersState } from "./types.ts";

const CHECKED = "checked.json";
const CHECKED_KEY = "buzz-reminders-checked.v1";

function readChecked(scope: string): number | undefined {
  const raw = readJson<Record<string, string>>(CHECKED, {})[`${CHECKED_KEY}:${scope}`];
  return raw && /^\d{1,12}$/.test(raw) ? Number(raw) : undefined;
}

function writeChecked(scope: string, at: number): void {
  try {
    const all = readJson<Record<string, string>>(CHECKED, {});
    all[`${CHECKED_KEY}:${scope}`] = String(at);
    writeJson(CHECKED, all);
  } catch (error) {
    // The in-memory time still prevents repeats while this runs.
    log("could not save the reminder check time:", error);
  }
}

interface Community {
  status?: string;
  scope?: string | null;
  relay?: string;
  live?: boolean;
  liveStatus?: { roster?: string };
  error?: string | null;
}

interface Channel {
  id: string;
  name?: string;
  kind?: string;
}

const idle: RemindersState = { status: "loading", hydrated: false, reminders: [] };

export class Hub {
  community: Community | undefined;
  state: RemindersState = idle;
  now = nowSeconds();
  private channels: { status?: string; channels?: Channel[] } = {};
  private scope = "";
  private watches: { reminders?: string; channels?: string } = {};
  private baselines = new Map<string, number>();
  private notified = new Map<string, Set<string>>();
  private saved: string | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private markers = "";
  private badge: string | null | undefined;
  /** Bumped on every rebind; a watch answered for an older one is dropped. */
  private generation = 0;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly host: Connection) {}

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  private changed(): void {
    for (const listener of this.listeners) listener();
  }

  /** The selected community, or its connection, changed. */
  async bind(community: Community): Promise<void> {
    this.community = community;
    const scope = community.status === "ready" && community.scope ? community.scope : "";
    if (scope === this.scope) {
      this.check();
      return;
    }
    const generation = ++this.generation;
    this.unwatch();
    this.scope = scope;
    this.state = idle;
    this.channels = {};
    this.publishMarkers();
    if (!scope) {
      this.check();
      return;
    }
    if (!this.baselines.has(scope)) {
      const checked = readChecked(scope) ?? nowSeconds();
      this.baselines.set(scope, checked);
      writeChecked(scope, checked);
    }
    try {
      const [reminders, channels] = await Promise.all([
        this.host.request<{ watch: string }>("reminders/watch"),
        this.host.request<{ watch: string }>("session/watch", { what: "channelList" }),
      ]);
      if (generation !== this.generation) {
        void this.host.request("reminders/unwatch", { watch: reminders.watch }).catch(() => {});
        void this.host.request("session/unwatch", { watch: channels.watch }).catch(() => {});
        return;
      }
      this.watches = { reminders: reminders.watch, channels: channels.watch };
    } catch (error) {
      log("could not follow reminders:", error);
    }
    this.check();
  }

  private unwatch(): void {
    if (this.watches.reminders)
      void this.host.request("reminders/unwatch", { watch: this.watches.reminders }).catch(() => {});
    if (this.watches.channels)
      void this.host.request("session/unwatch", { watch: this.watches.channels }).catch(() => {});
    this.watches = {};
  }

  /** `reminders/changed`. */
  reminders(params: Params): void {
    if (params.watch !== this.watches.reminders) return;
    this.state = fromList(params);
    this.publishMarkers();
    this.check();
  }

  /** `session/changed` for the channel list. */
  channelList(params: Params): void {
    if (params.watch !== this.watches.channels) return;
    this.channels = params.value ?? {};
    this.check();
  }

  /** Messages with a pending reminder show the alarm after their byline. */
  private publishMarkers(): void {
    const ids = [...pendingMessageIds(this.state)].sort();
    const key = ids.join(",");
    if (key === this.markers) return;
    this.markers = key;
    this.host.notify("message/markers", { action: "remind-me", messages: ids });
  }

  private publishBadge(): void {
    const due = countDue(this.state.reminders, this.now);
    const text = due ? String(due) : null;
    if (text === this.badge) return;
    this.badge = text;
    this.host.notify("page/badge", { page: "reminders", text });
  }

  check(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.now = nowSeconds();
    this.publishBadge();
    this.changed();
    const community = this.community;
    // Nothing notifies before the first history read succeeds.
    if (!this.scope || !community || community.status !== "ready" || !this.state.hydrated) return;
    // Notification access is judged against the channel list, so hold until a
    // full roster read has verified it.
    if (this.channels.status !== "ready" || community.liveStatus?.roster !== "verified") return;
    const now = this.now;
    const scope = this.scope;
    const baseline = this.baselines.get(scope) ?? now;
    const shown = this.notified.get(scope) ?? new Set<string>();
    this.notified.set(scope, shown);
    const keyOf = (reminder: Reminder) => `${reminder.id}:${reminder.notBefore}`;
    for (const reminder of dueSince(this.state.reminders, baseline, now)) {
      const sourceKey = keyOf(reminder);
      if (shown.has(sourceKey)) continue;
      shown.add(sourceKey);
      const message = navigableTarget(reminder);
      const channel = message && this.channels.channels?.find((item) => item.id === message.channelId);
      void this.host
        .request("notifications/submit", {
          category: "reminders",
          sourceKey,
          ...(message ? { channel: message.channelId, message: message.eventId } : { page: "buzz.reminders/reminders" }),
          ...(community.relay ? { community: community.relay } : {}),
          title: channel && channel.kind !== "dm" && channel.name ? `Reminder due · #${channel.name}` : "Reminder due",
          body: [reminder.target?.preview, reminder.note].find((text) => text?.trim()) ?? "A reminder is waiting",
        })
        .catch((error) => log("could not notify a reminder:", error));
    }
    // The saved time is what the next launch treats as seen, so it moves only
    // while live and after a read made during this connection succeeded.
    if (this.state.status === "ready" && this.state.error === undefined && community.live) {
      const key = `${scope}:${now}`;
      if (key !== this.saved) {
        writeChecked(scope, now);
        this.saved = key;
      }
    }
    // After sleep the timer fires late; the baseline still covers the gap.
    const delay = nextDelay(this.state.reminders, now);
    if (delay !== undefined) this.timer = setTimeout(() => this.check(), delay);
  }
}
