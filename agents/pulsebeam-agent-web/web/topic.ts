import type {
  Agent,
  AgentEvent,
  Topic,
  TopicMode,
  TopicRegistration,
  TopicCallbacks,
  TopicConsumerError,
  TopicEvent,
} from "./types.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const MAX_PAYLOAD = 65_536;
const MAX_BUFFERED = 256;

type Entry = {
  name: string;
  mode: TopicMode;
  publisher: boolean;
  pendingPublishes: number;
  subscribers: Set<{ close(): void }>;
};

function requireFields(value: object, allowed: readonly string[]): void {
  if (Object.keys(value).some((field) => !allowed.includes(field)))
    throw new TypeError("unknown topic option");
}

type Pending<T> = {
  resolve: (value: IteratorResult<TopicEvent<T>>) => void;
  reject: (reason: unknown) => void;
};

function decodeEvent<T>(
  event: AgentEvent,
  entry: Entry,
): TopicEvent<T> | undefined {
  if (!("topic" in event) || event.topic !== entry.name) return;
  if (event.type === "topic-message" && event.mode === entry.mode) {
    return Object.freeze({
      type: "message",
      dataTrackId: event.dataTrackId,
      sequence: event.sequence,
      participantId: event.participantId,
      topic: event.topic,
      value: JSON.parse(decoder.decode(event.payload)) as T,
    });
  }
  if (event.type === "topic-recovery-gap" && entry.mode === "ordered") {
    return Object.freeze({
      type: "gap",
      dataTrackId: event.dataTrackId,
      participantId: event.participantId,
      topic: event.topic,
      expectedSequence: event.expectedSequence,
      availableSequence: event.availableSequence,
    });
  }
}

function channelFailure(event: AgentEvent, entry: Entry): boolean {
  return (
    event.type === "topic-channel-failed" &&
    event.direction === "subscribe" &&
    event.topic === entry.name &&
    event.mode === entry.mode
  );
}

class TopicIterator<T> implements AsyncIterableIterator<TopicEvent<T>> {
  readonly #registry: TopicRegistry;
  readonly #entry: Entry;
  readonly #queue: TopicEvent<T>[] = [];
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

  [Symbol.asyncIterator](): AsyncIterableIterator<TopicEvent<T>> {
    return this;
  }

  next(): Promise<IteratorResult<TopicEvent<T>>> {
    if (this.#failure !== undefined) return Promise.reject(this.#failure);
    if (this.#queue.length > 0)
      return Promise.resolve({ value: this.#queue.shift()!, done: false });
    if (this.#closed) return Promise.resolve({ value: undefined, done: true });
    if (this.#pending.length === MAX_BUFFERED) {
      const error = new RangeError("topic consumer has too many pending reads");
      this.close(error);
      return Promise.reject(error);
    }
    return new Promise((resolve, reject) =>
      this.#pending.push({ resolve, reject }),
    );
  }

  async return(): Promise<IteratorResult<TopicEvent<T>>> {
    this.close();
    return { value: undefined, done: true };
  }

  async throw(error: unknown): Promise<IteratorResult<TopicEvent<T>>> {
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
    this.#registry.release(this.#entry, this);
    for (const pending of this.#pending.splice(0)) {
      if (error !== undefined) pending.reject(error);
      else pending.resolve({ value: undefined, done: true });
    }
  }

  readonly #abort = (): void => this.close();

  readonly #event = (raw: AgentEvent): void => {
    if (this.#closed) return;
    if (
      channelFailure(raw, this.#entry) &&
      raw.type === "topic-channel-failed"
    ) {
      this.close(new Error(raw.message));
      return;
    }
    let event: TopicEvent<T> | undefined;
    try {
      event = decodeEvent<T>(raw, this.#entry);
    } catch (error) {
      this.close(error);
      return;
    }
    if (!event) return;
    const pending = this.#pending.shift();
    if (pending) {
      pending.resolve({ value: event, done: false });
      return;
    }
    if (this.#entry.mode === "latest") {
      const previous = this.#queue.findIndex(
        (queued) => queued.dataTrackId === event.dataTrackId,
      );
      if (previous !== -1) this.#queue.splice(previous, 1);
    }
    if (this.#queue.length === MAX_BUFFERED) {
      if (this.#entry.mode === "lossy") this.#queue.shift();
      else if (this.#entry.mode === "latest") {
        this.close(
          new RangeError("topic consumer cannot retain all latest tracks"),
        );
        return;
      }
    }
    this.#queue.push(event);
    if (this.#queue.length > MAX_BUFFERED) this.#compactOrdered();
  };

  #compactOrdered(): void {
    const firstByTrack = new Map<string, number>();
    for (let second = 0; second < this.#queue.length; second += 1) {
      const later = this.#queue[second]!;
      const first = firstByTrack.get(later.dataTrackId);
      if (first === undefined) {
        firstByTrack.set(later.dataTrackId, second);
        continue;
      }
      const earlier = this.#queue[first]!;
      const expectedSequence =
        earlier.type === "gap" ? earlier.expectedSequence : earlier.sequence;
      const end =
        later.type === "gap" ? later.availableSequence : later.sequence + 1n;
      const availableSequence =
        earlier.type === "gap" && earlier.availableSequence > end
          ? earlier.availableSequence
          : end;
      this.#queue[first] = Object.freeze({
        type: "gap",
        dataTrackId: earlier.dataTrackId,
        participantId: earlier.participantId,
        topic: earlier.topic,
        expectedSequence,
        availableSequence,
      });
      this.#queue.splice(second, 1);
      return;
    }
    this.close(
      new RangeError("topic consumer overflow cannot retain all track gaps"),
    );
  }
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

  topic<T>(name: string, options: { readonly mode: TopicMode }): Topic<T> {
    if (this.#closed) throw new Error("agent is closed");
    if (typeof name !== "string" || !/^[A-Za-z0-9_-]{1,85}$/.test(name))
      throw new TypeError("invalid topic name");
    if (!["latest", "lossy", "ordered"].includes(options?.mode))
      throw new TypeError("invalid topic mode");
    requireFields(options, ["mode"]);
    const mode = options.mode;
    const key = `${mode}:${name}`;
    const existing = this.#facades.get(key);
    if (existing) return existing as Topic<T>;
    const entry: Entry = {
      name,
      mode,
      publisher: false,
      pendingPublishes: 0,
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
        if (entry.pendingPublishes === MAX_BUFFERED)
          throw new RangeError("topic publish admission queue is full");
        if (!entry.publisher) {
          entry.publisher = true;
          try {
            this.#sync();
          } catch (error) {
            entry.publisher = false;
            throw error;
          }
        }
        entry.pendingPublishes += 1;
        try {
          await this.#send(name, mode, payload);
        } finally {
          entry.pendingPublishes -= 1;
        }
      },
      subscribe: (options?: {
        readonly signal?: AbortSignal;
      }): AsyncIterable<TopicEvent<T>> => {
        if (this.#closed) throw new Error("agent is closed");
        if (options !== undefined) requireFields(options, ["signal"]);
        if (options?.signal?.aborted)
          return { async *[Symbol.asyncIterator]() {} };
        const iterator = new TopicIterator<T>(this, entry, options?.signal);
        entry.subscribers.add(iterator);
        try {
          this.#sync();
        } catch (error) {
          iterator.close();
          throw error;
        }
        return iterator;
      },
    });
    this.#facades.set(key, facade as Topic<unknown>);
    return facade;
  }

  listen<T>(
    name: string,
    options: { readonly mode: TopicMode },
    callbacks: TopicCallbacks<T>,
  ): () => void {
    requireFields(callbacks, ["onEvent", "onError"]);
    if (
      typeof callbacks.onEvent !== "function" ||
      (callbacks.onError !== undefined &&
        typeof callbacks.onError !== "function")
    )
      throw new TypeError("invalid topic callbacks");
    this.topic<T>(name, options);
    const entry = this.#entries.get(`${options.mode}:${name}`)!;
    let live = true;
    const report = (error: TopicConsumerError) => {
      if (callbacks.onError) callbacks.onError(error);
      else console.error("PulseBeam topic consumer failed", error);
    };
    const consumer = {
      close: () => {
        if (!live) return;
        live = false;
        unsubscribe();
        this.release(entry, consumer);
      },
    };
    const unsubscribe = this.agent.subscribeEvents((raw) => {
      if (!live) return;
      if (channelFailure(raw, entry) && raw.type === "topic-channel-failed") {
        consumer.close();
        report({ code: "channel-failed", event: raw });
        return;
      }
      let event: TopicEvent<T> | undefined;
      try {
        event = decodeEvent<T>(raw, entry);
      } catch (cause) {
        consumer.close();
        report({ code: "decode-failed", topic: name, cause });
        return;
      }
      if (event) callbacks.onEvent(event);
    });
    entry.subscribers.add(consumer);
    try {
      this.#sync();
    } catch (error) {
      consumer.close();
      throw error;
    }
    return consumer.close;
  }

  release(entry: Entry, consumer: { close(): void }): void {
    if (entry.subscribers.delete(consumer)) this.#sync();
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const entry of this.#entries.values())
      for (const consumer of [...entry.subscribers]) consumer.close();
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
