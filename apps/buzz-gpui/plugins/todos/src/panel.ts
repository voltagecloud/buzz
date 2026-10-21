// The Todos panel beside a channel (buzz-app `TodosPanel.tsx`): the
// channel Canvas's `## Todos` list, grouped by status, with a status and an
// assignee per todo. Edits change only their own bytes of the Canvas and
// save at once over the revision they were read from. The draft, the text
// it started from, its base revision and the new todo's text are kept
// locally until saved, so a conflict or a restart loses nothing.

import { type Connection, type Element, el, log, type Params, readJson, RpcError, type ViewOpen, writeJson } from "@buzz/plugin-sdk";
import { addTodo, assignTodo, readTodos, type Status, setStatus } from "./model.ts";
import { publicKeyLabels } from "@buzz/plugin-sdk/identity";

export type Draft = { content: string; original: string; base: string | null; input: string };
const empty: Draft = { content: "", original: "", base: null, input: "" };

const statuses: [Status, string][] = [
  ["todo", "To do"],
  ["doing", "In progress"],
  ["done", "Done"],
];
const statusIcon = (status: Status) =>
  status === "done"
    ? { icon: "CheckCircle", iconTone: "complete" as const }
    : status === "doing"
      ? { icon: "CircleHalf", iconTone: "progress" as const }
      : { icon: "Circle", iconTone: "subtle" as const };

const CONFLICT =
  "Canvas changed elsewhere. Refresh to load the latest todos. Your changes are kept until then.";
const ABOUT =
  "Todos are shared in this channel’s Canvas. Changes save automatically. Refresh to load others’ changes. On older relays, simultaneous saves can overwrite edits.";
/** `canvas/save`'s error when the Canvas moved on (`CanvasConflictError`). */
const CANVAS_CONFLICT = 5;

const DRAFTS = "drafts.json";
type Drafts = Record<string, Record<string, Draft>>;

/** A saved draft, or undefined when absent or malformed (`readDraft`). */
export function parseDraft(value: unknown): Draft | undefined {
  const draft = value as Partial<Draft> | null | undefined;
  return draft &&
    typeof draft.content === "string" &&
    typeof draft.original === "string" &&
    typeof draft.input === "string" &&
    (draft.base === null || typeof draft.base === "string")
    ? (draft as Draft)
    : undefined;
}

export const draftKey = (channel: string) => `todos-draft-v1:${channel}`;

function readDraft(scope: string, key: string): Draft | undefined {
  return parseDraft(readJson<Drafts>(DRAFTS, {})[scope]?.[key]);
}

/** Saves the draft while it holds anything, else forgets it (`writeView`). */
function writeDraft(scope: string, key: string, draft: Draft | null): void {
  const all = readJson<Drafts>(DRAFTS, {});
  const drafts = { ...all[scope] };
  if (draft) drafts[key] = draft;
  else delete drafts[key];
  if (Object.keys(drafts).length) all[scope] = drafts;
  else delete all[scope];
  writeJson(DRAFTS, all);
}

const message = (reason: unknown) => (reason instanceof Error ? reason.message : String(reason));

interface Name {
  name: string;
  source: string | null;
}

export class TodosPanel {
  private readonly key: string;
  private saved: Draft | undefined;
  private draft: Draft;
  private inputVersion = 1;
  private loaded = false;
  private busy: "load" | "save" | undefined;
  private error = "";
  private conflict = false;
  private available: boolean | undefined;
  private operation = 0;
  private saving = false;
  private savedAt = 0;
  private cancelWait: (() => void) | undefined;
  private members: string[] | undefined;
  private listError = false;
  private names: Record<string, Name> = {};
  private namesError = false;
  private people = "";
  private watches: { kind: "session" | "names"; id: string }[] = [];
  private closed = false;
  private writeTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly host: Connection,
    private readonly view: string,
    private readonly open: ViewOpen,
    private readonly update: () => void,
  ) {
    this.key = draftKey(this.channel);
    this.saved = readDraft(this.scope, this.key);
    this.draft = this.saved ?? empty;
    void this.start();
  }

  private get channel(): string {
    return this.open.channel ?? "";
  }

  private get scope(): string {
    return typeof this.open.scope === "string" ? this.open.scope : "";
  }

  private active(): boolean {
    return !this.closed;
  }

  private async start(): Promise<void> {
    if (!this.channel) return;
    this.host.request<boolean>("canvas/available").then(
      (available) => {
        this.available = available;
        this.update();
      },
      () => {
        this.available = false;
        this.update();
      },
    );
    try {
      const watch = await this.host.request<{ watch: string }>("session/watch", { what: "channelList" });
      if (this.closed) void this.host.request("session/unwatch", { watch: watch.watch }).catch(() => {});
      else this.watches.push({ kind: "session", id: watch.watch });
    } catch (error) {
      log("could not follow the channel list:", error);
      this.listError = true;
    }
    await this.load(false);
  }

  /** `session/changed` for the channel list. */
  channelList(value: Params): void {
    const channel = (value?.channels as { id: string; members?: string[] }[] | undefined)?.find(
      (channel) => channel.id === this.channel,
    );
    this.members = channel?.members;
    this.listError = value?.status === "error";
    this.followNames();
    this.update();
  }

  /** `names/changed` for this panel's people. */
  namesChanged(names: Record<string, Name>): void {
    this.names = { ...this.names, ...names };
    this.update();
  }

  ownsWatch(id: string): boolean {
    return this.watches.some((watch) => watch.id === id);
  }

  close(): void {
    this.closed = true;
    this.operation++;
    this.cancelWait?.();
    if (this.writeTimer) {
      clearTimeout(this.writeTimer);
      this.flushDraft();
    }
    for (const watch of this.watches) {
      const method = watch.kind === "session" ? "session/unwatch" : "names/unwatch";
      void this.host.request(method, { watch: watch.id }).catch(() => {});
    }
  }

  /** Everyone the panel names: the members and the assignees. */
  private peopleOf(): string[] {
    const items = this.parsed().parsed?.items ?? [];
    return [
      ...new Set([...(this.members ?? []), ...items.flatMap((item) => (item.assignee ? [item.assignee.pubkey] : []))]),
    ].sort();
  }

  private followNames(retry = false): void {
    const people = this.peopleOf();
    const key = people.join(":");
    if (key === this.people && !retry) return;
    this.people = key;
    const old = this.watches.filter((watch) => watch.kind === "names");
    this.watches = this.watches.filter((watch) => watch.kind !== "names");
    for (const watch of old) void this.host.request("names/unwatch", { watch: watch.id }).catch(() => {});
    if (!people.length) return;
    // Profiles load in the background; names follow them.
    void this.host.request("profiles/get", { pubkeys: people.slice(0, 500) }).catch(() => {});
    this.host
      .request<{ watch: string; names: Record<string, Name> }>("names/watch", {
        pubkeys: people.slice(0, 500),
        candidates: (this.members ?? []).slice(0, 500),
      })
      .then(
        (answer) => {
          if (this.closed || this.people !== key) {
            void this.host.request("names/unwatch", { watch: answer.watch }).catch(() => {});
            return;
          }
          this.watches.push({ kind: "names", id: answer.watch });
          this.namesError = false;
          this.namesChanged(answer.names ?? {});
        },
        () => {
          this.namesError = true;
          this.update();
        },
      );
  }

  /** `resolveIdentity`: a key's start does not count as a name. */
  private resolveName(pubkey: string, fallback: string): string {
    const known = this.names[pubkey.toLowerCase()];
    return known?.source ? known.name : fallback;
  }

  private setDraft(next: Draft): void {
    this.draft = next;
    // Typing saves once it pauses; edits save at once.
    const keep = next.content !== next.original || next.input ? next : null;
    if (this.writeTimer) clearTimeout(this.writeTimer);
    this.pending = keep;
    this.writeTimer = setTimeout(() => this.flushDraft(), 300);
  }

  private pending: Draft | null = null;

  private flushDraft(): void {
    this.writeTimer = undefined;
    try {
      writeDraft(this.scope, this.key, this.pending);
    } catch (error) {
      log("could not keep the todos draft:", error);
    }
  }

  private async load(replace: boolean): Promise<void> {
    if (!this.active()) return;
    const generation = ++this.operation;
    this.busy = "load";
    this.error = "";
    this.update();
    const saved = this.saved;
    const savedDirty = !!saved && saved.content !== saved.original;
    try {
      const head = await this.host.request<{ id: string; createdAt: number; content: string } | null>("canvas/read", {
        channel: this.channel,
      });
      if (!this.active() || generation !== this.operation) return;
      this.savedAt = head?.createdAt ?? 0;
      this.loaded = true;
      const alreadySaved = !!saved && saved.content === head?.content;
      const changed = !replace && savedDirty && !alreadySaved && saved.base !== (head?.id ?? null);
      this.conflict = changed;
      if (changed) this.error = CONFLICT;
      else if (!replace && savedDirty && !alreadySaved) this.error = "Recovered unsaved changes. Retry to save them to Canvas.";
      if (replace || !savedDirty || alreadySaved) {
        this.setDraft({
          ...empty,
          input: !replace ? (saved?.input ?? "") : "",
          content: head?.content ?? "",
          original: head?.content ?? "",
          base: head?.id ?? null,
        });
        this.inputVersion++;
      }
      // Later loads are refreshes: the recovered draft is spent.
      this.saved = undefined;
      this.followNames();
    } catch (reason) {
      if (this.active() && generation === this.operation) this.error = message(reason);
    } finally {
      if (this.active() && generation === this.operation) this.busy = undefined;
      this.update();
    }
  }

  private async save(next = this.draft): Promise<void> {
    if (
      !this.active() ||
      this.saving ||
      this.busy ||
      !this.loaded ||
      next.content === next.original ||
      this.conflict ||
      this.available !== true
    )
      return;
    this.saving = true;
    const generation = ++this.operation;
    this.busy = "save";
    this.error = "";
    this.update();
    try {
      // Canvas revisions use whole seconds. Wait once, without retrying a write.
      const delay = (this.savedAt + 1) * 1000 - Date.now();
      if (delay > 0) {
        await new Promise<void>((resolve) => {
          const timer = setTimeout(resolve, Math.min(delay, 1000));
          this.cancelWait = () => {
            clearTimeout(timer);
            resolve();
          };
        });
        this.cancelWait = undefined;
      }
      if (!this.active() || generation !== this.operation) return;
      const head = await this.host.request<{ id: string; createdAt: number; content: string }>("canvas/save", {
        channel: this.channel,
        content: next.content,
        expected: next.base,
      });
      if (!this.active() || generation !== this.operation) return;
      this.savedAt = head.createdAt;
      this.setDraft({ ...this.draft, original: head.content, content: head.content, base: head.id });
    } catch (reason) {
      if (this.active() && generation === this.operation) {
        const conflict = reason instanceof RpcError && reason.code === CANVAS_CONFLICT;
        this.conflict = conflict;
        this.error = conflict ? CONFLICT : `Couldn’t save. Your changes are still here. ${message(reason)}`;
      }
    } finally {
      this.saving = false;
      if (this.active() && generation === this.operation) this.busy = undefined;
      this.update();
    }
  }

  private parsed() {
    try {
      return { parsed: readTodos(this.draft.content), parseError: "" };
    } catch (reason) {
      return { parsed: undefined, parseError: message(reason) };
    }
  }

  private disabledState() {
    const { parseError } = this.parsed();
    const inputDisabled = !this.loaded || this.busy === "load" || this.available !== true || !!parseError || this.conflict;
    return { inputDisabled, disabled: inputDisabled || this.busy === "save" };
  }

  private change(edit: () => string): void {
    if (!this.active() || this.disabledState().disabled || this.saving) return;
    try {
      const next = { ...this.draft, content: edit() };
      this.setDraft(next);
      void this.save(next);
    } catch (reason) {
      this.error = message(reason);
    }
  }

  private add(): void {
    if (!this.active() || this.disabledState().disabled || this.saving || !this.draft.input.trim()) return;
    try {
      const next = { ...this.draft, content: addTodo(this.draft.content, this.draft.input), input: "" };
      this.inputVersion++;
      this.setDraft(next);
      void this.save(next);
    } catch (reason) {
      this.error = message(reason);
    }
  }

  private async refresh(): Promise<void> {
    if (this.busy) return;
    const dirty = this.draft.content !== this.draft.original;
    if (this.loaded && (dirty || this.draft.input)) {
      await this.confirmRefresh();
      return;
    }
    await this.load(this.loaded);
  }

  private async confirmRefresh(): Promise<void> {
    const discard = await this.host
      .request<boolean>("ui/confirm", {
        title: "Discard unsaved changes?",
        description: "Refreshing replaces your local changes and new todo text with the saved channel Canvas.",
        confirm: "Discard and refresh",
        cancel: "Keep editing",
        destructive: true,
      })
      .catch(() => false);
    if (discard) await this.load(true);
  }

  async event(id: string, event: string, value: unknown): Promise<void> {
    if (id === "new" && event === "change" && typeof value === "string") {
      this.setDraft({ ...this.draft, input: value });
      return;
    }
    if ((id === "new" && event === "submit") || (id === "add" && event === "click")) {
      if (id === "new" && typeof value === "string") this.draft = { ...this.draft, input: value };
      this.add();
      return;
    }
    if (id === "refresh") return this.refresh();
    if (id === "conflict-refresh") return this.confirmRefresh();
    if (id === "retry") return this.save();
    if (id === "close") {
      void this.host.request("ui/closeView", { view: this.view }).catch(() => {});
      return;
    }
    if (id === "retry-users") {
      void this.host.request("community/refresh", { what: "channels" }).catch(() => {});
      this.followNames(true);
      return;
    }
    const match = /^todo-(\d+)-(check|status|assignee)$/.exec(id);
    if (!match) return;
    const offset = Number(match[1]);
    const item = this.parsed().parsed?.items.find((item) => item.offset === offset);
    if (!item) return;
    if (match[2] === "check") {
      this.change(() => setStatus(this.draft.content, offset, value ? "done" : "todo"));
    } else if (match[2] === "status") {
      const status = statuses.find(([status]) => status === value)?.[0];
      if (!status || status === item.status) return;
      this.change(() => setStatus(this.draft.content, offset, status));
    } else {
      const pubkey = typeof value === "string" ? value : "";
      // Membership is checked again at the moment of choosing.
      if (!this.members || (pubkey && !this.members.includes(pubkey))) return;
      const labels = publicKeyLabels(this.peopleOf());
      this.change(() =>
        assignTodo(
          this.draft.content,
          offset,
          pubkey ? { pubkey, name: this.resolveName(pubkey, labels.get(pubkey) ?? pubkey) } : undefined,
        ),
      );
    }
  }

  render(): Element {
    if (!this.channel) return el.text("Connect to this channel to view todos.", { tone: "subtle" });
    const { parsed, parseError } = this.parsed();
    const { inputDisabled, disabled } = this.disabledState();
    const dirty = this.draft.content !== this.draft.original;
    const items = parsed?.items ?? [];
    const people = this.peopleOf();
    const keyLabels = publicKeyLabels(people);
    const userLabel = (pubkey: string, fallback = "") => {
      const key = keyLabels.get(pubkey) ?? pubkey;
      const name = this.resolveName(pubkey, fallback);
      return name ? `${name} · ${key}` : key;
    };
    const choices = (this.members ?? [])
      .map((pubkey) => ({ value: pubkey, label: userLabel(pubkey) }))
      .sort((a, b) => a.label.localeCompare(b.label));
    const saveStatus =
      this.busy === "save" ? "Saving…" : dirty ? "Unsaved changes" : this.loaded ? "Saved in Canvas" : "Channel Canvas";
    const feedback = (this.error || parseError || (dirty && !this.busy)) && [
      (this.error || parseError) && el.text(this.error || parseError, { style: "small", tone: "subtle" }),
      this.conflict && !this.busy && el.button("conflict-refresh", "Refresh todos", { size: "sm", variant: "outline" }),
      dirty &&
        !this.busy &&
        !this.conflict &&
        !parseError &&
        el.button("retry", "Retry", { size: "sm", variant: "outline", disabled }),
    ];
    const group = ([status, title]: [Status, string]) => {
      const group = items.filter((item) => item.status === status);
      if (!group.length) return false;
      return el.column(
        [
          el.row([el.text(title, { style: "label" }), el.badge(String(group.length))], { gap: "sm" }),
          ...group.map((item) =>
            el.row(
              [
                el.row(
                  [
                    el.checkbox(`todo-${item.offset}-check`, item.status === "done", {
                      label: item.label,
                      indeterminate: item.status === "doing",
                      completed: item.status === "done",
                      disabled,
                    }),
                  ],
                  { grow: true },
                ),
                el.menu(
                  `todo-${item.offset}-status`,
                  statuses.map(([value, label]) => ({ value, label, ...statusIcon(value) })),
                  { ...statusIcon(item.status), tooltip: "Status", title: "Change status", value: item.status, disabled },
                ),
                el.menu(
                  `todo-${item.offset}-assignee`,
                  [
                    { value: "", label: "Unassigned", icon: "User" },
                    ...choices.map((choice) => ({
                      value: choice.value,
                      label: userLabel(choice.value, item.assignee?.pubkey === choice.value ? item.assignee.name : ""),
                      avatar: choice.value,
                    })),
                    ...(item.assignee && !this.members?.includes(item.assignee.pubkey)
                      ? [
                          {
                            value: item.assignee.pubkey,
                            label: `${userLabel(item.assignee.pubkey, item.assignee.name)} (${this.members ? "not in channel" : "membership unavailable"})`,
                            avatar: item.assignee.pubkey,
                            disabled: true,
                          },
                        ]
                      : []),
                  ],
                  {
                    ...(item.assignee ? { avatar: item.assignee.pubkey } : { icon: "User" }),
                    tooltip: item.assignee ? userLabel(item.assignee.pubkey, item.assignee.name) : "Assign task",
                    title: "Assign to",
                    value: item.assignee?.pubkey ?? "",
                    disabled: disabled || !this.members,
                  },
                ),
              ],
              { gap: "sm" },
            ),
          ),
        ],
        { gap: "sm" },
      );
    };
    return el.column(
      [
        el.header("Todos", {
          icon: "ListChecks",
          subtitle: typeof this.open.channelName === "string" ? this.open.channelName : undefined,
          actions: [
            el.popover("about", { icon: "Info", tooltip: "About todos", title: saveStatus, description: ABOUT }),
            el.button("refresh", "", {
              icon: "ArrowsClockwise",
              size: "sm",
              tooltip: "Refresh todos",
              loading: this.busy === "load",
              disabled: !!this.busy,
            }),
            el.button("close", "", { icon: "X", size: "sm", variant: "ghost", tooltip: "Hide todos" }),
          ],
        }),
        el.column(
          [
            el.row(
              [
                el.input("new", {
                  placeholder: "Add a todo…",
                  value: this.draft.input,
                  version: this.inputVersion,
                  disabled: inputDisabled,
                  grow: true,
                }),
                el.button("add", "Add", { variant: "prominent", disabled: disabled || !this.draft.input.trim() }),
              ],
              { gap: "sm" },
            ),
            feedback && el.column(feedback, { gap: "sm" }),
            (this.namesError || this.listError || !this.members) &&
              el.row(
                [
                  el.text(
                    !this.members || this.listError
                      ? "Channel members unavailable or out of date."
                      : "Names unavailable; public keys still identify users.",
                    { style: "small", tone: "subtle" },
                  ),
                  el.button("retry-users", "Retry users", { size: "sm", variant: "ghost" }),
                ],
                { gap: "sm", wrap: true },
              ),
            this.available === false && el.text("This connection doesn’t support saving Canvas.", { style: "small", tone: "subtle" }),
            !this.loaded
              ? el.text(this.busy ? "Loading todos…" : "Couldn’t load todos. Try Refresh.", { style: "small", tone: "subtle" })
              : !parseError &&
                el.column(
                  [
                    items.length === 0 &&
                      el.empty("No todos yet", {
                        icon: "ListChecks",
                        description: "Add a todo above to track work with this channel. Changes save automatically.",
                      }),
                    ...statuses.map(group),
                  ],
                  { gap: "lg" },
                ),
          ],
          { gap: "md", padding: "md" },
        ),
      ],
      { gap: "none" },
    );
  }
}
