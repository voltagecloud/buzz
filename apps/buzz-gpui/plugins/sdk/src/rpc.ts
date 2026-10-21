// JSON-RPC 2.0, one message per line, as Buzz speaks it to a plugin
// (`docs/plugins.md`, "How Buzz talks to a plugin").

export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json | undefined };

/** Params and results as they come off the wire. */
export type Params = any; // eslint-disable-line @typescript-eslint/no-explicit-any

/** An error Buzz answered a request with, or one to answer Buzz with. */
export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
  ) {
    super(message);
    this.name = "RpcError";
  }
}

/** JSON-RPC's "method not found". */
export const METHOD_NOT_FOUND = -32601;
/** JSON-RPC's "internal error". */
export const INTERNAL_ERROR = -32603;

type RequestHandler = (params: Params) => unknown;
type NotificationHandler = (params: Params) => unknown;

interface Pending {
  resolve(value: unknown): void;
  reject(error: Error): void;
}

/** One end of the line-delimited JSON-RPC link. */
export class Connection {
  private next = 1;
  private readonly pending = new Map<number, Pending>();
  private readonly requests = new Map<string, RequestHandler>();
  private readonly notifications = new Map<string, NotificationHandler[]>();

  constructor(private readonly write: (line: string) => void) {}

  /** Answers requests for `method`; the handler's result (or a promise of it) is the answer. */
  handle(method: string, handler: RequestHandler): void {
    this.requests.set(method, handler);
  }

  /** Hears notifications for `method`. */
  on(method: string, handler: NotificationHandler): () => void {
    const list = this.notifications.get(method) ?? [];
    list.push(handler);
    this.notifications.set(method, list);
    return () => {
      const at = list.indexOf(handler);
      if (at >= 0) list.splice(at, 1);
    };
  }

  /** Asks the other end; rejects with an `RpcError` when it answers with one. */
  request<T = unknown>(method: string, params: Params = {}): Promise<T> {
    const id = this.next++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  notify(method: string, params: Params = {}): void {
    this.send({ jsonrpc: "2.0", method, params });
  }

  /** Handles one line from the other end. */
  receive(line: string): void {
    if (!line.trim()) return;
    let message: Params;
    try {
      message = JSON.parse(line);
    } catch {
      log("ignored a line that is not JSON");
      return;
    }
    const { id, method, params } = message;
    if (typeof method === "string" && id !== undefined && id !== null) {
      void this.answer(id, method, params ?? {});
    } else if (typeof method === "string") {
      for (const handler of [...(this.notifications.get(method) ?? [])]) {
        Promise.resolve()
          .then(() => handler(params ?? {}))
          .catch((error) => log(`${method} failed:`, error));
      }
    } else if (typeof id === "number") {
      const pending = this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id);
      if (message.error) {
        pending.reject(new RpcError(message.error.code ?? 0, message.error.message ?? "Request failed"));
      } else {
        pending.resolve(message.result ?? null);
      }
    }
  }

  /** Fails every request still waiting, as when the link closes. */
  close(): void {
    for (const pending of this.pending.values()) pending.reject(new RpcError(0, "Buzz closed the link."));
    this.pending.clear();
  }

  private async answer(id: Json, method: string, params: Params): Promise<void> {
    const handler = this.requests.get(method);
    if (!handler) {
      this.send({ jsonrpc: "2.0", id, error: { code: METHOD_NOT_FOUND, message: `No ${method} here.` } });
      return;
    }
    try {
      const result = await handler(params);
      this.send({ jsonrpc: "2.0", id, result: result === undefined ? null : result });
    } catch (error) {
      const code = error instanceof RpcError ? error.code : INTERNAL_ERROR;
      const text = error instanceof Error ? error.message : String(error);
      this.send({ jsonrpc: "2.0", id, error: { code, message: text } });
    }
  }

  private send(message: Params): void {
    this.write(JSON.stringify(message));
  }
}

/** Logs to stderr, which Buzz keeps; stdout is only for messages. */
export function log(...parts: unknown[]): void {
  const text = parts
    .map((part) => (part instanceof Error ? (part.stack ?? part.message) : typeof part === "string" ? part : JSON.stringify(part)))
    .join(" ");
  process.stderr.write(`${text}\n`);
}

/** The link over this process's stdin and stdout. The process exits when stdin closes. */
export async function stdio(): Promise<Connection> {
  const { createInterface } = await import("node:readline");
  const connection = new Connection((line) => {
    process.stdout.write(`${line}\n`);
  });
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  lines.on("line", (line) => connection.receive(line));
  lines.on("close", () => {
    connection.close();
    process.exit(0);
  });
  return connection;
}
