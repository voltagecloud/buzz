// A stand-in for Buzz, for testing a plugin without the app: it speaks
// the same JSON-RPC to the plugin, keeps each view's last tree, and
// answers the plugin's requests with handlers the test gives it.

import type { Element } from "./elements.ts";
import { type PluginSpec, serve, type ViewOpen } from "./plugin.ts";
import { Connection, type Params } from "./rpc.ts";

export class FakeHost {
  /** The plugin's side, as `serve` gets it. */
  readonly plugin: Connection;
  /** Buzz's side. */
  readonly buzz: Connection;
  /** The last tree each view sent. */
  readonly trees = new Map<string, Element>();
  /** Notifications the plugin sent, other than trees. */
  readonly sent: { method: string; params: Params }[] = [];
  private next = 0;

  constructor(spec: PluginSpec) {
    // Delivered a turn later, as a pipe would.
    this.plugin = new Connection((line) => queueMicrotask(() => this.buzz.receive(line)));
    this.buzz = new Connection((line) => queueMicrotask(() => this.plugin.receive(line)));
    const record = this.buzz.receive.bind(this.buzz);
    this.buzz.receive = (line: string) => {
      const message = JSON.parse(line);
      if (message.method === "view/render") this.trees.set(message.params.view, message.params.tree);
      else if (message.method && message.id === undefined) this.sent.push({ method: message.method, params: message.params });
      record(line);
    };
    serve(spec, this.plugin);
  }

  /** Answers the plugin's `method` requests. */
  answer(method: string, handler: (params: Params) => unknown): this {
    this.buzz.handle(method, handler);
    return this;
  }

  initialize(): Promise<Params> {
    return this.buzz.request("initialize", { apiVersion: 1, app: { name: "Buzz", version: "test" }, host: { browser: false } });
  }

  /** Opens a view of `contribution` and waits for its first tree. */
  async open(contribution: string, fields: Partial<ViewOpen> = {}): Promise<string> {
    const view = fields.view ?? `v${++this.next}`;
    this.buzz.notify("view/open", { kind: "page", target: null, channel: null, ...fields, view, contribution });
    await this.settle();
    return view;
  }

  /** Sends an event to a view and waits for it to draw. */
  async event(view: string, id: string, event: string, value?: unknown): Promise<void> {
    this.buzz.notify("view/event", { view, id, event, value });
    await this.settle();
  }

  notify(method: string, params: Params): Promise<void> {
    this.buzz.notify(method, params);
    return this.settle();
  }

  tree(view: string): Element {
    const tree = this.trees.get(view);
    if (!tree) throw new Error(`${view} has drawn nothing`);
    return tree;
  }

  /** Lets queued messages and the work they start run. */
  async settle(): Promise<void> {
    for (let turn = 0; turn < 20; turn++) await new Promise((resolve) => setImmediate(resolve));
  }
}

/** Every element in `tree`, depth first. */
export function elements(tree: Element): Element[] {
  const out: Element[] = [tree];
  for (const child of (tree.children as Element[] | undefined) ?? []) out.push(...elements(child));
  return out;
}

/** The text a tree shows, for asserting on. */
export function texts(tree: Element): string[] {
  return elements(tree).flatMap((element) =>
    typeof element.text === "string" ? [element.text] : typeof element.title === "string" ? [element.title] : [],
  );
}

/** The element with `id`. */
export function byId(tree: Element, id: string): Element | undefined {
  return elements(tree).find((element) => element.id === id);
}
