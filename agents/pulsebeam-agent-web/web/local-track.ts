import type { CapturedTrack } from "./capture-source.js";

export class LocalTrackCapacityError extends RangeError {
  readonly kind: "video" | "audio";
  readonly label: string;
  readonly capacity: number;

  constructor(kind: "video" | "audio", label: string, capacity: number) {
    super(`no reserved ${kind} slot remains for ${label}`);
    this.name = "LocalTrackCapacityError";
    this.kind = kind;
    this.label = label;
    this.capacity = capacity;
  }
}

export class LocalTrackHandle<
  K extends "video" | "audio",
  S extends CapturedTrack,
> {
  #source: S | null = null;
  #enabled = true;
  #closed = false;
  #listeners = new Set<() => void>();

  constructor(
    readonly kind: K,
    readonly label: string,
    private readonly apply: (source: S | null) => void,
    private readonly applyEnabled: (enabled: boolean) => void,
  ) {}

  get source(): S | null {
    return this.#source;
  }

  get enabled(): boolean {
    return this.#enabled;
  }

  setEnabled(enabled: boolean): void {
    if (this.#closed) throw new Error("agent is closed");
    if (typeof enabled !== "boolean")
      throw new TypeError("enabled must be a boolean");
    if (enabled === this.#enabled) return;
    this.applyEnabled(enabled);
    this.#enabled = enabled;
    for (const listener of this.#listeners) listener();
  }

  setSource(source: S | null): void {
    if (this.#closed) throw new Error("agent is closed");
    if (source === this.#source) return;
    this.apply(source);
    this.#source = source;
    for (const listener of this.#listeners) listener();
  }

  subscribe(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  clear(): void {
    this.#closed = true;
    this.#source = null;
    for (const listener of this.#listeners) listener();
    this.#listeners.clear();
  }
}
