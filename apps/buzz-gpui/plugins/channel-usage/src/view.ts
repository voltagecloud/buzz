// Channel usage (buzz-app `bundled/channel-usage`): the token and cost
// counters agents reported for this channel, read from your saved turn
// metrics. Usage is attributed by the channel an agent reported, never by
// thread. The scan stops at 2,000 records.

import { type Connection, type Element, el, log, type Params, type ViewOpen } from "@buzz/plugin-sdk";
import { formatPublicKey } from "@buzz/plugin-sdk/identity";
import {
  aggregateSessionUsage,
  type ArchivePage,
  type Counters,
  projectUsageWithUnreadable,
  type UsageRecord,
  type UsageSession,
} from "./usage.ts";

const MAX_RECORDS = 2000;

const number = (value?: number) => (value === undefined ? "—" : value.toLocaleString("en-US"));
const usd = (value?: number) => (value === undefined ? "—" : `$${value.toFixed(value < 0.01 && value > 0 ? 4 : 2)}`);
const date = (value: number) => new Date(value).toLocaleString("en-US");

export function total(record: UsageRecord | null): string {
  const counters = record?.cumulative;
  if (counters?.totalTokens !== undefined) return `${number(counters.totalTokens)} tokens`;
  if (counters?.inputTokens !== undefined || counters?.outputTokens !== undefined)
    return `Input ${number(counters.inputTokens)} / Output ${number(counters.outputTokens)}`;
  return "—";
}

const pair = (label: string, value: string) =>
  el.row([el.text(label, { style: "small", tone: "subtle" }), el.text(value, { style: "small" })], {
    justify: "between",
    gap: "md",
  });

function breakdown(counts: Counters | null | undefined): Element {
  return el.column(
    [
      pair("Provider total tokens", number(counts?.totalTokens)),
      pair("Input", number(counts?.inputTokens)),
      pair("Output", number(counts?.outputTokens)),
      pair("Cache reads", number(counts?.cacheReadTokens)),
      pair("Cache writes", number(counts?.cacheWriteTokens)),
      pair("Estimated USD", usd(counts?.costUsd)),
    ],
    { gap: "none" },
  );
}

const small = (text: string) => el.text(text, { style: "small", tone: "subtle" });

type Record_ = ArchivePage["records"][number];

/** One `archive/read` page. */
interface Page {
  records: { id: string; agent: string; createdAt: number; receivedAt: number; frame: unknown }[];
  before: number | null;
  skipped: number;
  revision: number;
}

/** A channel as the list gives it, for who may see its usage. */
interface Listed {
  id: string;
  cached?: boolean;
  readOnly?: boolean;
  archived?: boolean;
  members?: string[];
}

export class ChannelUsage {
  private allowed = false;
  private listCached = false;
  /** The selected community is this panel's, as the archive reads it. */
  private here = false;
  private listed: Listed | undefined;
  private status: "loading" | "ready" | "error" | "unavailable" = "loading";
  private records: Record_[] = [];
  private partial = false;
  private skipped = 0;
  private generation = 0;
  private pending = false;
  private agent: string | null = null;
  private selected: string | null = null;
  private expanded: string | null = null;
  private provenance = new Set<string>();
  private sessionDetails = false;
  private names: Record<string, { name: string; source: string | null }> = {};
  private members: string[] = [];
  private watches: string[] = [];
  private namesWatch: string | undefined;
  private namesKey = "";
  private closed = false;

  constructor(
    private readonly host: Connection,
    private readonly open: ViewOpen,
    private readonly update: () => void,
  ) {
    void this.start();
  }

  private get channel(): string {
    return this.open.channel ?? "";
  }

  private get viewer(): string {
    return typeof this.open.viewer === "string" ? this.open.viewer : "";
  }

  private async start(): Promise<void> {
    try {
      this.community(await this.host.request<Params>("community/current"));
    } catch (error) {
      log("could not read the community:", error);
    }
    try {
      const watch = await this.host.request<{ watch: string }>("session/watch", { what: "channelList" });
      if (this.closed) void this.host.request("session/unwatch", { watch: watch.watch }).catch(() => {});
      else this.watches.push(watch.watch);
    } catch (error) {
      log("could not follow the channel list:", error);
    }
  }

  ownsWatch(id: string): boolean {
    return this.watches.includes(id) || this.namesWatch === id;
  }

  /** `community/changed`: usage shows only for this panel's community, live. */
  community(current: Params): void {
    this.here =
      current?.status === "ready" &&
      current?.cached !== true &&
      current?.scope === this.open.scope &&
      current?.viewer === this.viewer;
    this.check();
  }

  /** `session/changed` for the channel list: who may see usage, as of now. */
  channelList(value: Params): void {
    this.listCached = value?.cached === true;
    this.listed = (value?.channels as Listed[] | undefined)?.find((channel) => channel.id === this.channel);
    this.members = this.listed?.members ?? [];
    this.check();
  }

  private check(): void {
    const channel = this.listed;
    const allowed =
      this.here &&
      !!channel &&
      !this.listCached &&
      !channel.cached &&
      !channel.readOnly &&
      !channel.archived &&
      !!channel.members?.includes(this.viewer);
    if (allowed !== this.allowed) {
      this.allowed = allowed;
      // Losing access forgets what was read; regaining it reads again.
      this.reset();
      if (allowed) void this.load();
    }
    this.update();
  }

  namesChanged(names: Params): void {
    this.names = { ...this.names, ...names };
    this.update();
  }

  close(): void {
    this.closed = true;
    this.generation++;
    for (const watch of this.watches) void this.host.request("session/unwatch", { watch }).catch(() => {});
    if (this.namesWatch) void this.host.request("names/unwatch", { watch: this.namesWatch }).catch(() => {});
  }

  private reset(): void {
    this.generation++;
    this.pending = false;
    this.records = [];
    this.partial = false;
    this.skipped = 0;
    this.status = "loading";
  }

  private fail(): void {
    this.reset();
    this.status = "error";
  }

  /** `useUsageArchive`: pages of saved metrics, all from one archive revision. */
  private async load(): Promise<void> {
    if (!this.allowed || this.pending) return;
    this.pending = true;
    const token = this.generation;
    const live = () => !this.closed && token === this.generation && this.allowed;
    this.status = "loading";
    this.update();
    let records: Record_[] = [];
    let scanned = 0;
    let skipped = 0;
    let before: number | null = null;
    let revision: number | null = null;
    try {
      while (scanned < MAX_RECORDS) {
        const settings = await this.host.request<{ revision: number }>("archive/settings");
        if (!live()) return;
        if (revision !== null && settings.revision !== revision) return this.fail();
        const page: Page = await this.host.request<Page>("archive/read", {
          kind: 44200,
          ...(before !== null ? { before } : {}),
        });
        if (!live()) return;
        // Reject mixed revisions before a fallible trailing settings read.
        if (page.revision !== settings.revision || (revision !== null && page.revision !== revision)) return this.fail();
        const latest = await this.host.request<{ revision: number }>("archive/settings");
        if (!live()) return;
        if (page.revision !== latest.revision) return this.fail();
        revision = page.revision;
        records = [
          ...records,
          ...page.records.map((record) => ({
            id: record.id,
            agent: record.agent,
            createdAt: record.createdAt,
            receivedAt: record.receivedAt,
            plaintext: typeof record.frame === "string" ? record.frame : JSON.stringify(record.frame),
          })),
        ].slice(0, MAX_RECORDS);
        skipped += page.skipped;
        scanned += page.records.length + page.skipped;
        // A non-advancing cursor must not cause an unbounded read loop.
        if (page.before !== null && before !== null && page.before >= before) {
          this.status = "error";
          return;
        }
        if (page.before === null || scanned >= MAX_RECORDS) {
          this.records = records;
          this.skipped = skipped;
          this.status = "ready";
          this.partial = page.before !== null && scanned >= MAX_RECORDS;
          this.followNames();
          return;
        }
        before = page.before;
      }
    } catch (error) {
      log("could not read saved usage:", error);
      if (live()) this.status = "error";
    } finally {
      if (token === this.generation) this.pending = false;
      this.update();
    }
  }

  private projection() {
    return this.allowed ? projectUsageWithUnreadable(this.records, this.channel) : { groups: [], unreadable: 0 };
  }

  private followNames(): void {
    const agents = [...new Set(this.projection().groups.map((group) => group.agent))].sort();
    const key = `${agents.join(":")}|${[...this.members].sort().join(":")}`;
    if (key === this.namesKey || !agents.length) return;
    this.namesKey = key;
    if (this.namesWatch) void this.host.request("names/unwatch", { watch: this.namesWatch }).catch(() => {});
    this.namesWatch = undefined;
    this.host
      .request<{ watch: string; names: Params }>("names/watch", {
        pubkeys: agents.slice(0, 500),
        candidates: this.members.slice(0, 500),
      })
      .then(
        (answer) => {
          if (this.closed || this.namesKey !== key) {
            void this.host.request("names/unwatch", { watch: answer.watch }).catch(() => {});
            return;
          }
          this.namesWatch = answer.watch;
          this.namesChanged(answer.names ?? {});
        },
        (error) => log("could not name the agents:", error),
      );
  }

  /** `useChannelIdentityNames`: a key's start does not count as a name. */
  private label(id: string): string {
    const known = this.names[id.toLowerCase()];
    return known?.source ? known.name : (formatPublicKey(id) ?? "Agent");
  }

  event(id: string, value: unknown): void {
    if (id === "agent" && typeof value === "string") {
      this.agent = value;
      this.selected = null;
    } else if (id === "session" && typeof value === "string") this.selected = value;
    else if (id === "refresh") {
      this.reset();
      void this.load();
    } else if (id === "close") this.agent = null;
    else if (id === "session-details") this.sessionDetails = !this.sessionDetails;
    else if (id.startsWith("turn:")) {
      const turn = id.slice(5);
      this.expanded = this.expanded === turn ? null : turn;
    } else if (id.startsWith("provenance:")) {
      const turn = id.slice(11);
      if (!this.provenance.delete(turn)) this.provenance.add(turn);
    }
  }

  private turns(group: UsageSession): Element {
    return el.column(
      [
        el.text(`Recorded turns · ${group.turns.length} · newest first`, { style: "label" }),
        el.scroll(
          group.turns.map((row) => {
            const open = this.expanded === row.id;
            return el.column(
              [
                el.row(
                  [
                    el.text(
                      row.turnId?.endsWith(":initial") ? "Session setup" : row.turnSeq === null ? "—" : `#${row.turnSeq}`,
                      { style: "small" },
                    ),
                    el.text(date(row.timestamp), { style: "small", tone: "subtle" }),
                    el.row([el.text(number(row.turn?.totalTokens), { style: "small" })], { grow: true, justify: "end" }),
                    el.button(`turn:${row.id}`, `${usd(row.turn?.costUsd)} · ${open ? "−" : "+"}`, {
                      size: "sm",
                      variant: "ghost",
                    }),
                  ],
                  { gap: "md" },
                ),
                open &&
                  el.column(
                    [
                      row.conflict
                        ? small("Conflicting turn records. Per-turn usage unavailable.")
                        : !row.turn
                          ? small("Per-turn usage unavailable.")
                          : breakdown(row.turn),
                      el.button(
                        `provenance:${row.id}`,
                        `${this.provenance.has(row.id) ? "▾" : "▸"} Identifiers and provenance`,
                        { size: "sm", variant: "ghost" },
                      ),
                      this.provenance.has(row.id) &&
                        el.column(
                          [
                            pair("Session", row.sessionId ?? "Not reported"),
                            pair("Turn", row.turnId ?? "Not reported"),
                            pair("Harness / model", `${row.harness} / ${row.model ?? "Not reported"}`),
                            pair("Stop reason", row.stopReason ?? "Not reported"),
                            pair("Pricing identity", row.pricing ?? "Not reported"),
                          ],
                          { gap: "none" },
                        ),
                    ],
                    { gap: "sm", padding: "sm" },
                  ),
              ],
              { gap: "none" },
            );
          }),
          { maxHeight: 320 },
        ),
      ],
      { gap: "sm" },
    );
  }

  render(): Element {
    if (!this.allowed) return el.column([]);
    const { groups: all, unreadable } = this.projection();
    const agents = [...new Set(all.map((group) => group.agent))];
    const activeAgent = this.agent && agents.includes(this.agent) ? this.agent : null;
    const groups = all.filter((group) => group.agent === activeAgent);
    const group = groups.find((group) => group.key === this.selected) ?? (groups.length === 1 ? groups[0] : undefined);
    const totals = aggregateSessionUsage(groups);
    const incomplete = this.skipped || unreadable;
    return el.column(
      [
        el.row(
          [
            el.text("Channel session usage", { style: "label" }),
            el.select(
              "agent",
              agents.map((id) => {
                const sessions = all.filter((group) => group.agent === id);
                return {
                  value: id,
                  label: `${this.label(id)} · ${sessions.length === 1 ? total(sessions[0]?.latest ?? null) : `${sessions.length} sessions`}`,
                };
              }),
              { value: activeAgent ?? undefined, placeholder: "Select agent", width: 280 },
            ),
            this.status === "loading" && small("Loading saved usage…"),
            el.button("refresh", "Refresh", { size: "sm", variant: "ghost" }),
          ],
          { gap: "sm", wrap: true },
        ),
        activeAgent &&
          el.card(
            [
              el.row(
                [
                  el.text(`Session usage · ${this.label(activeAgent)}`, { style: "title" }),
                  el.button("close", "Close", { size: "sm", variant: "ghost" }),
                ],
                { justify: "between" },
              ),
              el.column(
                [
                  el.text(`Totals across ${groups.length} ${groups.length === 1 ? "session" : "sessions"}`, {
                    style: "label",
                  }),
                  small(
                    "Latest reported counters per session, not thread attribution. Cache counts are part of input. — means not reported for every session.",
                  ),
                  totals.complete < groups.length &&
                    small(
                      `Incomplete: ${totals.complete} of ${groups.length} sessions have trustworthy cumulative counters.`,
                    ),
                  breakdown(totals.counters),
                ],
                { gap: "sm" },
              ),
              el.select(
                "session",
                groups.map((entry, index) => ({
                  value: entry.key,
                  label: `${entry.sessionId ? "Session" : "Unidentified session"} ${index + 1} · ${entry.turns.length} ${entry.turns.length === 1 ? "turn" : "turns"} · ${entry.turns[0] ? date(entry.turns[0].timestamp) : "No turns"}`,
                })),
                { value: group?.key, placeholder: "Select session" },
              ),
              group &&
                el.column(
                  [
                    small(
                      `${group.sessionId ? `Session ${group.sessionId}` : "Unidentified session"} · ${group.latest?.harness ?? group.turns[0]?.harness ?? "Not reported"} / ${group.latest?.model ?? group.turns[0]?.model ?? "Not reported"}`,
                    ),
                    el.text("Latest reported session counters", { style: "label" }),
                    small(`Last reported ${group.latest ? date(group.latest.timestamp) : "Not reported"}`),
                    !group.latest && small("Session counters unavailable."),
                    breakdown(group.latest?.cumulative),
                    small("Cache counts are part of input, not additional tokens."),
                    el.button(
                      "session-details",
                      `${this.sessionDetails ? "▾" : "▸"} Session identity and pricing provenance`,
                      { size: "sm", variant: "ghost" },
                    ),
                    this.sessionDetails &&
                      small(`${group.sessionId ?? "Not reported"} · ${group.latest?.pricing ?? "Not reported"}`),
                    this.turns(group),
                  ],
                  { gap: "sm" },
                ),
            ],
            { gap: "md" },
          ),
        this.status === "error" && el.text("Could not read saved usage. Use Refresh to retry.", { style: "small", tone: "danger" }),
        !agents.length &&
          this.status === "ready" &&
          small(
            incomplete
              ? "Some saved metrics could not be read. Usage history is incomplete."
              : this.partial
                ? "No matching records in the saved history scanned."
                : "No matching records in the saved history searched.",
          ),
        !!agents.length && !!incomplete && small("Some saved metrics could not be read. Usage history is incomplete."),
        this.partial && small("Partial history: 2,000-record scan limit reached."),
      ],
      { gap: "md" },
    );
  }
}
