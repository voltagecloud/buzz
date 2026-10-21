// One conversation collection's pages, with its own cursor and recovery
// (buzz-app's `useConversationSource`).

import { log } from "@buzz/plugin-sdk";
import { type ConversationEntry, type ConversationSource, loadConversationPage } from "./conversation.ts";

export class Source {
  entries: ConversationEntry[] = [];
  next: number | undefined;
  loading = true;
  error: string | undefined;
  private page = 1;
  private controller = new AbortController();

  constructor(
    private readonly url: string,
    private readonly source: ConversationSource,
    private readonly changed: () => void,
  ) {
    void this.load();
  }

  private async load(): Promise<void> {
    const controller = new AbortController();
    this.controller = controller;
    this.loading = true;
    this.error = undefined;
    this.changed();
    try {
      const { entries, next } = await loadConversationPage(
        this.url,
        this.source,
        this.page,
        AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]),
      );
      if (controller.signal.aborted) return;
      this.entries = [...new Map([...this.entries, ...entries].map((entry) => [entry.id, entry])).values()];
      this.next = next;
    } catch (error) {
      if (controller.signal.aborted) return;
      log(`could not load ${this.source}:`, error);
      this.error = error instanceof Error ? error.message : String(error);
    }
    this.loading = false;
    this.changed();
  }

  loadMore(): void {
    if (!this.loading && this.next) {
      this.page = this.next;
      void this.load();
    }
  }

  retry(): void {
    if (!this.loading) void this.load();
  }

  stop(): void {
    this.controller.abort();
  }
}
