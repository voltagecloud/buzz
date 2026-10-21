// Feedback (`bundled/feedback`): "Send feedback" in the account menu.
// Feedback goes to the deployment's private inbox through the session's
// durable outbox, so the dialog follows the feedback on its way: a send
// survives closing the dialog or the app and is retried as the same event.

import { type Connection, type Element, el, type Params, type PluginSpec, type ViewContext } from "@buzz/plugin-sdk";

const DESCRIPTION =
  "Feedback text goes to this Buzz deployment's feedback inbox, not a channel. Images and optional diagnostics use ordinary media storage, not inbox-only access; people allowed to read media on this deployment may access them if they have the link. Images upload when selected; diagnostics upload when you send. Removing an attachment or closing this form does not delete uploaded files.";
const PLACEHOLDER = "Tell us what went wrong, or share general feedback.";
const DIAGNOSTICS_DETAIL =
  "Includes capture time, app version when available, platform, user agent, and language. No application logs.";
const UNAVAILABLE = "Connect to a Buzz deployment to send or retry feedback.";
const DISCARD_CONFIRM =
  "Discard this saved feedback locally? It may already have been delivered; discarding will not undo delivery or delete uploaded attachments.";

const categories = [
  ["bug", "Bug"],
  ["praise", "Praise"],
  ["needs-work", "Needs work"],
] as const;
type Category = (typeof categories)[number][0];

export interface Attachment {
  name: string;
  url: string;
  mime: string;
  size: number;
  sha256: string;
}

interface Pending {
  id: string;
  content: string;
  delivery: "sending" | "accepted" | "seen" | "failed" | "unknown";
  error: string | null;
}

interface State {
  hydrated: boolean;
  available: boolean;
  error: string | null;
  pending: Pending | null;
}

/** `en_US.UTF-8` → `en-US`, like `navigator.language`. */
export function language(lang: string | undefined): string {
  const tag = lang?.split(/[.@]/)[0];
  return tag && tag !== "C" && tag !== "POSIX" ? tag.replace(/_/g, "-") : "unknown";
}

const os = (platform: string) => (platform === "darwin" ? "macos" : platform === "win32" ? "windows" : platform);
const arch = (machine: string) => (machine === "x64" ? "x86_64" : machine === "arm64" ? "aarch64" : machine);

/** The optional diagnostics file: no logs, only what identifies the build and environment. */
export function diagnostics(version: string, now = new Date(), env = process.env): string {
  const system = os(process.platform);
  return [
    "Buzz feedback diagnostics",
    `captured: ${now.toISOString()}`,
    `app version: ${version}`,
    `platform: ${system} ${arch(process.arch)}`,
    `user agent: buzz-gpui/${version} (${system})`,
    `language: ${language(env.LANG)}`,
  ].join("\n");
}

const message = (reason: unknown, fallback: string) => (reason instanceof Error && reason.message ? reason.message : fallback);

class FeedbackDialog {
  private state: State = { hydrated: false, available: false, error: null, pending: null };
  private category: Category | null = null;
  private text = "";
  private textVersion = 1;
  private image: Attachment | undefined;
  /** Kept so a second send reuses it instead of uploading again. */
  private uploadedDiagnostics: Attachment | undefined;
  private includeDiagnostics = false;
  private busy = false;
  private error = "";
  /** Bumped by every operation and by closing; stale results are dropped. */
  private attempt = 0;
  private watch: string | undefined;
  private closed = false;

  constructor(
    private readonly host: Connection,
    private readonly view: ViewContext,
    private readonly version: () => string,
  ) {
    this.host.request<{ watch: string; state: State }>("feedback/watch").then(
      (answer) => {
        if (this.closed) {
          void this.host.request("feedback/unwatch", { watch: answer.watch }).catch(() => {});
          return;
        }
        this.watch = answer.watch;
        this.changed(answer.state);
      },
      (reason) => {
        this.error = message(reason, "Could not load saved feedback.");
        this.view.update();
      },
    );
  }

  ownsWatch(id: string): boolean {
    return this.watch === id;
  }

  changed(state: State): void {
    if (state.error && state.error !== this.state.error) this.error = state.error;
    this.state = state;
    this.view.update();
  }

  close(): void {
    this.closed = true;
    this.attempt++;
    if (this.watch) void this.host.request("feedback/unwatch", { watch: this.watch }).catch(() => {});
  }

  private get available(): boolean {
    return this.state.hydrated && this.state.available;
  }

  private dismiss(): void {
    this.attempt++;
    this.busy = false;
    void this.host.request("ui/closeView", { view: this.view.id }).catch(() => {});
  }

  private async attachImage(): Promise<void> {
    if (!this.available || this.busy) return;
    const paths = await this.host
      .request<string[]>("ui/pickFiles", { prompt: "Attach image", images: true })
      .catch((reason) => {
        this.error = message(reason, "Image upload failed.");
        return [];
      });
    const path = paths[0];
    if (!path || this.closed) return;
    const attempt = ++this.attempt;
    this.busy = true;
    this.error = "";
    this.view.update();
    try {
      const image = await this.host.request<Attachment>("feedback/upload", { path });
      if (attempt === this.attempt) this.image = image;
    } catch (reason) {
      if (attempt === this.attempt) this.error = message(reason, "Image upload failed.");
    } finally {
      if (attempt === this.attempt) this.busy = false;
      this.view.update();
    }
  }

  private async submit(): Promise<void> {
    if (!this.available || this.state.pending || this.busy || !this.text.trim()) return;
    const attempt = ++this.attempt;
    this.busy = true;
    this.error = "";
    this.view.update();
    try {
      const answer = await this.host.request<{ diagnostics: Attachment | null }>("feedback/send", {
        message: this.text,
        category: this.category,
        attachments: this.image ? [this.image] : [],
        diagnostics: this.includeDiagnostics ? (this.uploadedDiagnostics ?? diagnostics(this.version())) : null,
      });
      if (attempt === this.attempt && answer.diagnostics) this.uploadedDiagnostics = answer.diagnostics;
    } catch (reason) {
      if (attempt === this.attempt) this.error = message(reason, "Could not send feedback.");
    } finally {
      if (attempt === this.attempt) this.busy = false;
      this.view.update();
    }
  }

  private async retry(): Promise<void> {
    const pending = this.state.pending;
    if (!pending || !this.available || this.busy) return;
    this.error = "";
    try {
      await this.host.request("feedback/retry", { id: pending.id });
    } catch (reason) {
      this.error = message(reason, "Could not retry feedback.");
    }
  }

  /** Forgets the pending feedback and closes, ready for new feedback. */
  private async clearPending(): Promise<void> {
    const pending = this.state.pending;
    if (!pending || this.busy) return;
    this.busy = true;
    const attempt = this.attempt;
    this.view.update();
    try {
      await this.host.request("feedback/dismiss", { id: pending.id });
      if (attempt !== this.attempt) return;
      this.category = null;
      this.text = "";
      this.textVersion++;
      this.image = undefined;
      this.uploadedDiagnostics = undefined;
      this.includeDiagnostics = false;
      this.error = "";
      this.dismiss();
    } catch (reason) {
      if (attempt === this.attempt) this.error = message(reason, "Could not clear feedback.");
    } finally {
      if (attempt === this.attempt) this.busy = false;
      this.view.update();
    }
  }

  private async discard(): Promise<void> {
    const pending = this.state.pending;
    if (!pending || (pending.delivery !== "failed" && pending.delivery !== "unknown")) return;
    const confirmed = await this.host
      .request<boolean>("ui/confirm", {
        title: "Discard locally",
        description: DISCARD_CONFIRM,
        confirm: "Discard locally",
        destructive: true,
      })
      .catch(() => false);
    if (confirmed) await this.clearPending();
  }

  event(id: string, event: string, value: unknown): unknown {
    if (id === "message" && event === "change" && typeof value === "string") {
      this.text = value;
      return;
    }
    const category = categories.find(([name]) => name === id)?.[0];
    if (category) {
      if (!this.busy) this.category = this.category === category ? null : category;
      return;
    }
    switch (id) {
      case "close":
        return this.dismiss();
      case "send":
        return this.submit();
      case "done":
        return this.clearPending();
      case "retry":
        return this.retry();
      case "discard":
        return this.discard();
      case "image":
        return this.attachImage();
      case "remove-image":
        this.image = undefined;
        return;
      case "diagnostics":
        this.includeDiagnostics = value === true;
        return;
    }
  }

  private status(pending: Pending): string {
    switch (pending.delivery) {
      case "accepted":
      case "seen":
        return "Feedback received by the relay.";
      case "unknown":
        return "Delivery could not be confirmed. Retry the same feedback; do not submit a duplicate.";
      case "failed":
        return `Feedback was not delivered. ${pending.error ?? "Retry to send."}`;
      default:
        return "Sending feedback…";
    }
  }

  render(): Element {
    const pending = this.state.pending;
    const busy = this.busy;
    const available = this.available;
    const delivered = pending?.delivery === "accepted" || pending?.delivery === "seen";
    const recover = pending?.delivery === "failed" || pending?.delivery === "unknown";
    const form = el.column(
      [
        el.row(
          categories.map(([id, label]) =>
            el.button(id, label, { variant: this.category === id ? "prominent" : "subtle", disabled: busy }),
          ),
          { gap: "sm", wrap: true },
        ),
        el.column(
          [
            el.text("Your feedback", { style: "label" }),
            el.textarea("message", { value: this.text, version: this.textVersion, placeholder: PLACEHOLDER, disabled: busy }),
          ],
          { gap: "sm" },
        ),
        el.column(
          [
            el.text("Attach image (optional)", { style: "label" }),
            el.row([el.button("image", "Choose image", { disabled: !available || busy })]),
            this.image &&
              el.row(
                [
                  el.text(`Image uploaded: ${this.image.name}`),
                  el.button("remove-image", "Remove image", { disabled: busy }),
                ],
                { gap: "sm", wrap: true },
              ),
          ],
          { gap: "sm" },
        ),
        el.column(
          [
            el.checkbox("diagnostics", this.includeDiagnostics, { label: "Attach diagnostics", disabled: !available || busy }),
            el.text(DIAGNOSTICS_DETAIL, { style: "small" }),
          ],
          { gap: "sm" },
        ),
      ],
      { gap: "md" },
    );
    const actions = [
      el.button("close", "Close"),
      delivered
        ? el.button("done", "Done", { variant: "prominent", disabled: busy })
        : recover
          ? [
              el.button("discard", "Discard locally", { disabled: busy }),
              el.button("retry", "Retry same feedback", { variant: "prominent", disabled: !available || busy }),
            ]
          : !pending
            ? el.button("send", "Send feedback", {
                variant: "prominent",
                disabled: !available || !this.text.trim() || busy,
              })
            : null,
    ].flat();
    return el.column(
      [
        el.text(DESCRIPTION, { style: "small", tone: "subtle" }),
        pending
          ? el.column([el.text(pending.content), el.text(this.status(pending))], { gap: "md" })
          : form,
        !available && el.text(UNAVAILABLE),
        this.error && el.text(this.error, { tone: "danger" }),
        el.row(actions, { gap: "sm", justify: "end" }),
      ],
      { gap: "md" },
    );
  }
}

const dialogs = new Map<string, FeedbackDialog>();
let version = "unknown";

export const feedback: PluginSpec = {
  contributions: {
    accountActions: [{ id: "send", title: "Send feedback", icon: "ChatCircle" }],
  },
  setup(host, init) {
    if (typeof init?.app?.version === "string") version = init.app.version;
    host.on("feedback/changed", (params: Params) => {
      for (const dialog of dialogs.values()) if (dialog.ownsWatch(params.watch)) dialog.changed(params.state);
    });
  },
  views: {
    send: (_, view) => {
      const dialog = new FeedbackDialog(view.host, view, () => version);
      dialogs.set(view.id, dialog);
      return {
        render: () => dialog.render(),
        event: (event) => dialog.event(event.id, event.event, event.value),
        close: () => {
          dialogs.delete(view.id);
          dialog.close();
        },
      };
    },
  },
};
