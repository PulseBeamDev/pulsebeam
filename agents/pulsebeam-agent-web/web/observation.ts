import type { AgentEvent, AgentSnapshot } from "./types.js";

type Read<T> =
  | { readonly type: "value"; readonly value: T }
  | { readonly type: "lagged"; readonly skipped: bigint }
  | { readonly type: "closed" };

// Match the bounded native state/event observation channel, not media demand.
const CAPACITY = 64;
class Observation<T> {
  readonly #values: T[] = [];
  readonly #readers: ((read: Read<T>) => void)[] = [];
  #skipped = 0n;
  #release: (() => void) | undefined;
  constructor(
    public closed: boolean,
    release: () => void,
  ) {
    this.#release = closed ? undefined : release;
  }
  publish(value: T): void {
    if (this.closed) return;
    const reader = this.#readers.shift();
    if (reader) {
      reader({ type: "value", value });
      return;
    }
    if (this.#values.length === CAPACITY) {
      this.#values.shift();
      this.#skipped++;
    }
    this.#values.push(value);
  }
  next(): Promise<Read<T>> {
    if (this.closed) return Promise.resolve({ type: "closed" });
    if (this.#skipped) {
      const skipped = this.#skipped;
      this.#skipped = 0n;
      return Promise.resolve({ type: "lagged", skipped });
    }
    if (this.#values.length)
      return Promise.resolve({ type: "value", value: this.#values.shift()! });
    return new Promise((resolve) => this.#readers.push(resolve));
  }
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.#values.length = 0;
    this.#skipped = 0n;
    for (const reader of this.#readers.splice(0)) reader({ type: "closed" });
    const release = this.#release;
    this.#release = undefined;
    release?.();
  }
}

export type SnapshotUpdate =
  | { readonly type: "snapshot"; readonly snapshot: AgentSnapshot }
  | { readonly type: "lagged"; readonly skipped: bigint }
  | { readonly type: "closed" };
export type EventUpdate =
  | { readonly type: "event"; readonly event: AgentEvent }
  | { readonly type: "lagged"; readonly skipped: bigint }
  | { readonly type: "closed" };

export class SnapshotStream {
  readonly #observation: Observation<AgentSnapshot>;
  #initial: AgentSnapshot | undefined;
  constructor(observation: Observation<AgentSnapshot>, initial: AgentSnapshot) {
    this.#observation = observation;
    this.#initial = observation.closed ? undefined : initial;
  }
  isClosed(): boolean {
    return this.#observation.closed;
  }
  async next(): Promise<SnapshotUpdate> {
    const initial = this.#initial;
    this.#initial = undefined;
    if (this.isClosed()) return { type: "closed" };
    if (initial) return { type: "snapshot", snapshot: initial };
    const read = await this.#observation.next();
    if (this.isClosed()) return { type: "closed" };
    return read.type === "value"
      ? { type: "snapshot", snapshot: read.value }
      : read;
  }
  async close(): Promise<void> {
    this.#initial = undefined;
    this.#observation.close();
  }
}
export class EventStream {
  readonly #observation: Observation<AgentEvent>;
  constructor(observation: Observation<AgentEvent>) {
    this.#observation = observation;
  }
  isClosed(): boolean {
    return this.#observation.closed;
  }
  async next(): Promise<EventUpdate> {
    const read = await this.#observation.next();
    if (this.isClosed()) return { type: "closed" };
    return read.type === "value" ? { type: "event", event: read.value } : read;
  }
  async close(): Promise<void> {
    this.#observation.close();
  }
}

export function observeSnapshots(
  initial: AgentSnapshot,
  closed: boolean,
  release: () => void,
) {
  const observation = new Observation<AgentSnapshot>(closed, release);
  const stream = new SnapshotStream(observation, initial);
  return {
    stream,
    publish: (snapshot: AgentSnapshot) => observation.publish(snapshot),
    close: () => {
      void stream.close();
    },
  };
}
export function observeEvents(closed: boolean, release: () => void) {
  const observation = new Observation<AgentEvent>(closed, release);
  return {
    stream: new EventStream(observation),
    publish: (event: AgentEvent) => observation.publish(event),
    close: () => observation.close(),
  };
}
