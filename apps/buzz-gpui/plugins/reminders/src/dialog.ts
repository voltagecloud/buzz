// "Remind me" for one message (buzz-app `RemindDialog.tsx`): a preset or
// custom time, plus an optional note.

import { type Connection, type Element, el } from "@buzz/plugin-sdk";
import { formatDue, parseCustomDateTime, shiftedFrom, TIME_PRESETS, todayDateString } from "./model.ts";

const NOTE_LIMIT = 4096;

/** The message a dialog reminds about, as the action handed it over. */
export interface Target {
  message: string;
  channel: string;
  preview: string;
  author: string;
}

export class RemindDialog {
  private note = "";
  private date = todayDateString();
  private time = "09:00";
  private pending = false;
  private error: string | undefined;

  constructor(
    private readonly host: Connection,
    private readonly view: string,
    private readonly target: Target | undefined,
    private readonly update: () => void,
  ) {}

  private close(): void {
    if (!this.pending) void this.host.request("ui/closeView", { view: this.view }).catch(() => {});
  }

  private async submit(notBefore: number): Promise<void> {
    if (!this.target || this.pending) return;
    this.pending = true;
    this.error = undefined;
    this.update();
    try {
      await this.host.request("reminders/create", {
        channel: this.target.channel,
        message: this.target.message,
        notBefore,
        note: this.note,
        preview: this.target.preview,
        author: this.target.author,
      });
      this.pending = false;
      this.close();
    } catch {
      this.error = "The reminder could not be saved. Try again.";
      this.pending = false;
    }
  }

  event(id: string, event: string, value: unknown): unknown {
    if (event === "change" && typeof value === "string") {
      if (id === "note") this.note = value.slice(0, NOTE_LIMIT);
      else if (id === "date") this.date = value;
      else if (id === "time") this.time = value;
      return;
    }
    if (id === "cancel") return this.close();
    if (id === "set") {
      const custom = parseCustomDateTime(this.date, this.time);
      return custom !== null ? this.submit(custom) : undefined;
    }
    const preset = /^preset:(\d+)$/.exec(id);
    const chosen = preset ? TIME_PRESETS[Number(preset[1])] : undefined;
    if (chosen) return this.submit(chosen.at());
  }

  render(): Element {
    if (!this.target) return el.text("This message can no longer be reminded about.", { tone: "subtle" });
    const pending = this.pending;
    const custom = parseCustomDateTime(this.date, this.time);
    return el.column(
      [
        el.text("Choose when to be reminded about this message.", { tone: "subtle" }),
        ...TIME_PRESETS.map((preset, index) => el.button(`preset:${index}`, preset.label, { disabled: pending, fill: true })),
        el.row(
          [
            el.input("date", { value: this.date, placeholder: "YYYY-MM-DD", grow: true }),
            el.input("time", { value: this.time, placeholder: "HH:MM", grow: true }),
          ],
          { gap: "sm" },
        ),
        custom !== null &&
          shiftedFrom(this.time, custom) &&
          el.text(`That time is skipped by daylight saving. The reminder will be set for ${formatDue(custom)}.`, {
            tone: "subtle",
          }),
        el.textarea("note", { placeholder: "Add a note (optional)", rows: 2, maxRows: 6 }),
        this.error && el.text(this.error, { tone: "subtle" }),
        el.row(
          [
            el.button("cancel", "Cancel", { disabled: pending }),
            el.button("set", "Set reminder", { variant: "prominent", disabled: pending || custom === null }),
          ],
          { gap: "sm", justify: "end" },
        ),
      ],
      { gap: "sm" },
    );
  }
}
