import { PulseBeamError, type Topic, type TopicDelivery, type TopicMode, type TopicOptions } from "./types.js";

export interface TopicOwner {
  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): void;
  closeTopic(topic: TopicController): void;
}

export class TopicController implements Topic {
  readonly name: string;
  readonly mode: TopicMode;
  readonly options: Readonly<TopicOptions>;
  #owner: TopicOwner | undefined;
  #listeners = new Set<(delivery: TopicDelivery) => void>();
  #items: TopicDelivery[] = [];
  #waiting: ((result: IteratorResult<TopicDelivery>) => void) | undefined;

  constructor(owner: TopicOwner, options: Readonly<TopicOptions>) {
    this.#owner = owner;
    this.options = options;
    this.name = options.name;
    this.mode = options.mode;
  }

  get closed(): boolean {
    return this.#owner === undefined;
  }

  send(payload: Uint8Array): void {
    if (!this.options.publish) {
      throw new PulseBeamError("invalid-command", `topic is not registered for publishing: ${this.name}`);
    }
    if (!this.#owner) throw new PulseBeamError("closed", `topic is closed: ${this.name}`);
    this.#owner.sendTopic(this.name, this.mode, payload);
  }

  subscribe(listener: (delivery: TopicDelivery) => void): () => void {
    if (!this.#owner) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  close(): void {
    this.#owner?.closeTopic(this);
  }

  [Symbol.asyncIterator](): AsyncIterator<TopicDelivery> {
    return {
      next: () => this.#next(),
      return: () => {
        this.close();
        return Promise.resolve({ value: undefined, done: true });
      },
    };
  }

  deliver(delivery: TopicDelivery): void {
    if (!this.#owner) return;
    for (const listener of [...this.#listeners]) {
      try {
        listener(delivery);
      } catch (error) {
        queueMicrotask(() => {
          throw error;
        });
      }
    }
    if (this.#waiting) {
      const resolve = this.#waiting;
      this.#waiting = undefined;
      resolve({ value: delivery, done: false });
    } else {
      this.#items.push(delivery);
    }
  }

  detach(): void {
    this.#owner = undefined;
    this.#listeners.clear();
    this.#items = [];
    if (this.#waiting) {
      this.#waiting({ value: undefined, done: true });
      this.#waiting = undefined;
    }
  }

  #next(): Promise<IteratorResult<TopicDelivery>> {
    const item = this.#items.shift();
    if (item) return Promise.resolve({ value: item, done: false });
    if (!this.#owner) return Promise.resolve({ value: undefined, done: true });
    if (this.#waiting) {
      return Promise.reject(
        new PulseBeamError("invalid-command", "only one async iterator may wait on a topic at a time"),
      );
    }
    return new Promise((resolve) => {
      this.#waiting = resolve;
    });
  }
}
