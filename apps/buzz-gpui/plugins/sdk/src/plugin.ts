// Runs a plugin: answers `initialize` with what it adds, keeps one object
// per open view, sends its tree again after each event, and routes Buzz's
// other notifications to the plugin's handlers.

import type { Element } from "./elements.ts";
import { Connection, log, type Params, stdio } from "./rpc.ts";

/** What `view/open` says about a view (`docs/plugins.md`, `view/open`). */
export interface ViewOpen {
  view: string;
  contribution: string;
  kind: string;
  target?: string | null;
  channel?: string | null;
  thread?: string | null;
  root?: string | null;
  community?: string | null;
  route?: unknown;
  [field: string]: unknown;
}

/** A click, toggle or typing in a view (`view/event`). */
export interface ViewEvent {
  view: string;
  id: string;
  event: string;
  value?: unknown;
}

/** One open view: what it draws and what it does with events. */
export interface View {
  render(): Element;
  event?(event: ViewEvent): unknown;
  /** `view/retarget`: a panel shows another target. */
  retarget?(params: Params): unknown;
  /** `page/route`: a page is visited at another route. */
  route?(params: Params): unknown;
  close?(): void;
}

export interface ViewContext {
  /** The view's id, for requests that name it. */
  readonly id: string;
  readonly host: Connection;
  /** Sends the view's tree again, such as once a request answers. */
  update(): void;
}

export type ViewFactory = (open: ViewOpen, context: ViewContext) => View;

export interface PluginSpec {
  /** The `initialize` answer: pages, panels and the rest. */
  contributions: Params | (() => Params);
  /** Makes a view for each contribution id that opens one. */
  views?: Record<string, ViewFactory>;
  /** Runs once, before `initialize` is answered, with its params (`app`, `host`): register handlers here. */
  setup?(host: Connection, init: Params): unknown;
}

interface Open {
  view: View;
  context: ViewContext;
  queued: boolean;
}

/** Runs `spec` over `host`. */
export function serve(spec: PluginSpec, host: Connection): void {
  const views = new Map<string, Open>();

  const render = (id: string) => {
    const open = views.get(id);
    if (!open) return;
    let tree: Element;
    try {
      tree = open.view.render();
    } catch (error) {
      log(`view ${id} failed to draw:`, error);
      tree = { type: "text", text: "Something went wrong drawing this view.", tone: "danger" };
    }
    host.notify("view/render", { view: id, tree });
  };

  // Several changes in one turn send one tree.
  const schedule = (id: string) => {
    const open = views.get(id);
    if (!open || open.queued) return;
    open.queued = true;
    queueMicrotask(() => {
      open.queued = false;
      if (views.get(id) === open) render(id);
    });
  };

  const settle = async (id: string, work: () => unknown) => {
    try {
      await work();
    } finally {
      schedule(id);
    }
  };

  let setup: Promise<unknown> | undefined;
  host.handle("initialize", async (params: Params) => {
    setup ??= Promise.resolve(spec.setup?.(host, params));
    await setup;
    return typeof spec.contributions === "function" ? spec.contributions() : spec.contributions;
  });

  host.on("view/open", (params: ViewOpen) => {
    const factory = spec.views?.[params.contribution];
    if (!factory) {
      log(`no view for ${params.contribution}`);
      return;
    }
    views.get(params.view)?.view.close?.();
    const context: ViewContext = { id: params.view, host, update: () => schedule(params.view) };
    const open: Open = { view: factory(params, context), context, queued: false };
    views.set(params.view, open);
    render(params.view);
  });

  host.on("view/event", (params: ViewEvent) => {
    const open = views.get(params.view);
    if (open?.view.event) return settle(params.view, () => open.view.event?.(params));
  });

  host.on("view/retarget", (params: Params) => {
    const open = views.get(params.view);
    if (open?.view.retarget) return settle(params.view, () => open.view.retarget?.(params));
  });

  host.on("page/route", (params: Params) => {
    const open = views.get(params.view);
    if (open?.view.route) return settle(params.view, () => open.view.route?.(params));
  });

  host.on("view/close", (params: { view: string }) => {
    const open = views.get(params.view);
    views.delete(params.view);
    open?.view.close?.();
  });
}

/** Runs `spec` over stdin and stdout: a plugin's `main`. */
export async function run(spec: PluginSpec): Promise<void> {
  process.on("uncaughtException", (error) => log("uncaught:", error));
  process.on("unhandledRejection", (error) => log("unhandled:", error));
  serve(spec, await stdio());
}
