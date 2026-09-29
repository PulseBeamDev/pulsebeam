import type {
  Agent,
  AgentEvent,
  Topic,
  TopicMode,
  TopicRegistration,
} from "./types.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const MAX_PAYLOAD = 65_536;
const MAX_BUFFERED = 256;

type Entry = {
  name: string;
  mode: TopicMode;
  publisher: boolean;
  subscribers: Set<TopicIterator<unknown>>;
};

type Pending<T> = {
  resolve: (value: IteratorResult<T>) => void;
  reject: (reason: unknown) => void;
};

class TopicIterator<T> implements AsyncIterableIterator<T> {
  readonly #registry: TopicRegistry;
  readonly #entry: Entry;
  readonly #queue: T[] = [];
  readonly #pending: Pending<T>[] = [];
  readonly #unsubscribe: () => void;
  readonly #signal?: AbortSignal;
  #closed = false;
  #failure: unknown;

  constructor(registry: TopicRegistry, entry: Entry, signal?: AbortSignal) {
    this.#registry = registry;
    this.#entry = entry;
    this.#signal = signal;
    this.#unsubscribe = registry.agent.subscribeEvents(this.#event);
    signal?.addEventListener("abort", this.#abort, { once: true });
  }

  [Symbol.asyncIterator](): AsyncIterableIterator<T> {
    return this;
  }

  next(): Promise<IteratorResult<T>> {
    if (this.#failure !== undefined) return Promise.reject(this.#failure);
    if (this.#queue.length > 0)
      return Promise.resolve({ value: this.#queue.shift()!, done: false });
    if (this.#closed) return Promise.resolve({ value: undefined, done: true });
    return new Promise((resolve, reject) =>
      this.#pending.push({ resolve, reject }),
    );
  }

  async return(): Promise<IteratorResult<T>> {
    this.close();
    return { value: undefined, done: true };
  }

  async throw(error: unknown): Promise<IteratorResult<T>> {
    this.close(error);
    throw error;
  }

  close(error?: unknown): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#failure = error;
    this.#queue.length = 0;
    this.#unsubscribe();
    this.#signal?.removeEventListener("abort", this.#abort);
    this.#registry.release(this.#entry, this as TopicIterator<unknown>);
    for (const pending of this.#pending.splice(0)) {
      if (error !== undefined) pending.reject(error);
      else pending.resolve({ value: undefined, done: true });
    }
  }

  readonly #abort = (): void => this.close();

  readonly #event = (event: AgentEvent): void => {
    if (
      this.#closed ||
      event.type !== "topic-message" ||
      event.topic !== this.#entry.name ||
      event.mode !== this.#entry.mode
    )
      return;
    let value: T;
    try {
      value = JSON.parse(decoder.decode(event.payload)) as T;
    } catch (error) {
      this.close(error);
      return;
    }
    const pending = this.#pending.shift();
    if (pending) {
      pending.resolve({ value, done: false });
    } else {
      if (this.#entry.mode === "latest") this.#queue.length = 0;
      if (this.#queue.length === MAX_BUFFERED) this.#queue.shift();
      this.#queue.push(value);
    }
  };
}

export class TopicRegistry {
  readonly agent: Agent;
  readonly #send: (
    name: string,
    mode: TopicMode,
    payload: Uint8Array,
  ) => Promise<void>;
  readonly #onChange: (registrations: readonly TopicRegistration[]) => void;
  readonly #entries = new Map<string, Entry>();
  readonly #facades = new Map<string, Topic<unknown>>();
  #closed = false;

  constructor(
    agent: Agent,
    send: (name: string, mode: TopicMode, payload: Uint8Array) => Promise<void>,
    onChange: (registrations: readonly TopicRegistration[]) => void,
  ) {
    this.agent = agent;
    this.#send = send;
    this.#onChange = onChange;
  }

  topic<T>(
    name: string,
    options: { readonly mode: "reliable" | "unreliable" },
  ): Topic<T> {
    if (this.#closed) throw new Error("agent is closed");
    if (!/^[A-Za-z0-9_-]{1,85}$/.test(name))
      throw new TypeError("invalid topic name");
    if (options.mode !== "reliable" && options.mode !== "unreliable")
      throw new TypeError("invalid topic mode");
    const mode: TopicMode = options.mode === "reliable" ? "ordered" : "latest";
    const key = `${mode}:${name}`;
    const existing = this.#facades.get(key);
    if (existing) return existing as Topic<T>;
    const entry: Entry = {
      name,
      mode,
      publisher: false,
      subscribers: new Set(),
    };
    this.#entries.set(key, entry);
    const facade: Topic<T> = Object.freeze({
      publish: async (value: T): Promise<void> => {
        if (this.#closed) throw new Error("agent is closed");
        const serialized = JSON.stringify(value);
        if (serialized === undefined)
          throw new TypeError("topic value is not JSON serializable");
        const payload = encoder.encode(serialized);
        if (payload.byteLength > MAX_PAYLOAD)
          throw new RangeError("topic payload exceeds 65536 bytes");
        if (!entry.publisher) {
          entry.publisher = true;
          this.#sync();
        }
        await this.#send(name, mode, payload);
      },
      subscribe: (subscriptionOptions?: {
        readonly signal?: AbortSignal;
      }): AsyncIterable<T> => {
        if (this.#closed) throw new Error("agent is closed");
        if (subscriptionOptions?.signal?.aborted)
          return { async *[Symbol.asyncIterator]() {} };
        const iterator = new TopicIterator<T>(
          this,
          entry,
          subscriptionOptions?.signal,
        );
        entry.subscribers.add(iterator as TopicIterator<unknown>);
        this.#sync();
        return iterator;
      },
    });
    this.#facades.set(key, facade as Topic<unknown>);
    return facade;
  }

  release(entry: Entry, iterator: TopicIterator<unknown>): void {
    if (!entry.subscribers.delete(iterator)) return;
    this.#sync();
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const entry of this.#entries.values())
      for (const iterator of [...entry.subscribers]) iterator.close();
    this.#entries.clear();
    this.#facades.clear();
  }

  #sync(): void {
    if (this.#closed) return;
    this.#onChange(
      [...this.#entries.values()]
        .filter((entry) => entry.publisher || entry.subscribers.size > 0)
        .map((entry) => ({
          name: entry.name,
          mode: entry.mode,
          publish: entry.publisher,
          subscribe: entry.subscribers.size > 0,
        })),
    );
  }
}
