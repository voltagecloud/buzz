// Reminders as buzz-app's relay layer types them
// (`features/relay/reminders.ts`), which the copied model uses.

export type ReminderStatus = "pending" | "done" | "cancelled";

export type ReminderTarget = Readonly<{
  eventId: string;
  channelId: string;
  preview: string;
  authorPubkey: string;
}>;

export type Reminder = Readonly<{
  /** The `d` coordinate. */
  id: string;
  eventId: string;
  createdAt: number;
  /** Due time in unix seconds; absent once done or cancelled. */
  notBefore?: number;
  status: ReminderStatus;
  target?: ReminderTarget;
  note?: string;
}>;

export type RemindersState = Readonly<{
  status: "loading" | "ready" | "error";
  /** True once a history read has succeeded; local saves and live arrivals don't count. */
  hydrated: boolean;
  reminders: readonly Reminder[];
  error?: string;
}>;

/** A `reminders/list` answer, as buzz-app's state. */
export function fromList(value: {
  status?: string;
  hydrated?: boolean;
  error?: string | null;
  reminders?: {
    id: string;
    eventId: string;
    createdAt: number;
    notBefore?: number | null;
    status: ReminderStatus;
    note?: string | null;
    message?: { id: string; channel: string; preview: string; author: string } | null;
  }[];
}): RemindersState {
  return {
    status: value.status === "ready" || value.status === "error" ? value.status : "loading",
    hydrated: value.hydrated === true,
    ...(value.error ? { error: value.error } : {}),
    reminders: (value.reminders ?? []).map((reminder) => ({
      id: reminder.id,
      eventId: reminder.eventId,
      createdAt: reminder.createdAt,
      ...(typeof reminder.notBefore === "number" ? { notBefore: reminder.notBefore } : {}),
      status: reminder.status,
      ...(reminder.note ? { note: reminder.note } : {}),
      ...(reminder.message
        ? {
            target: {
              eventId: reminder.message.id,
              channelId: reminder.message.channel,
              preview: reminder.message.preview,
              authorPubkey: reminder.message.author,
            },
          }
        : {}),
    })),
  };
}
