import { ErrorCode } from "./schema.js";
import { OperationError, raise } from "./agent-observation.js";

interface NativeCursor {
  next(): Promise<unknown>;
  close(): void;
}

export interface NativeResourceObserver {
  state(): unknown;
  updates(): NativeCursor;
}

interface RevisionedState {
  readonly observationRevision: bigint;
}

export interface ResourceUpdates<S> {
  next(): Promise<Readonly<S> | undefined>;
  close(): void;
}

function immutable<T>(value: T): T {
  if (Array.isArray(value)) return Object.freeze(value.map(immutable)) as T;
  if (value !== null && typeof value === "object") {
    return Object.freeze(
      Object.fromEntries(
        Object.entries(value).map(([key, child]) => [key, immutable(child)]),
      ),
    ) as T;
  }
  return value;
}

export class ResourceObservation<S extends RevisionedState> {
  #current: BoundResourceObservation<S> | undefined;

  constructor(readonly project: (value: S) => S = immutable) {}

  bind(observer: NativeResourceObserver): void {
    this.#current = new BoundResourceObservation<S>(observer, this.project);
  }

  #bound(): BoundResourceObservation<S> {
    if (!this.#current)
      throw new OperationError({
        code: ErrorCode.NotReady,
        message: "Resource runtime is inactive",
      });
    return this.#current;
  }

  state(): Readonly<S> {
    return this.#bound().state();
  }
  updates(): ResourceUpdates<S> {
    return this.#bound().updates();
  }
}

class BoundResourceObservation<S extends RevisionedState> {
  #value: Readonly<S> | undefined;
  readonly #observer: NativeResourceObserver;
  constructor(
    observer: NativeResourceObserver,
    readonly project: (value: S) => S,
  ) {
    this.#observer = observer;
  }

  state(): Readonly<S> {
    return this.accept(this.#observer.state() as S);
  }

  accept(value: S): Readonly<S> {
    if (this.#value?.observationRevision === value.observationRevision)
      return this.#value;
    const next = this.project(value);
    if (
      !this.#value ||
      value.observationRevision > this.#value.observationRevision
    )
      this.#value = next;
    return next;
  }

  updates(): ResourceUpdates<S> {
    this.state();
    try {
      return new ResourceUpdatesFacade(this.#observer.updates(), this);
    } catch (error) {
      return raise(error);
    }
  }
}

class ResourceUpdatesFacade<S extends RevisionedState>
  implements ResourceUpdates<S>
{
  readonly #cursor: NativeCursor;
  readonly #observation: BoundResourceObservation<S>;
  constructor(cursor: NativeCursor, observation: BoundResourceObservation<S>) {
    this.#cursor = cursor;
    this.#observation = observation;
  }

  async next(): Promise<Readonly<S> | undefined> {
    try {
      const value = (await this.#cursor.next()) as S | undefined;
      return value === undefined ? undefined : this.#observation.accept(value);
    } catch (error) {
      return raise(error);
    }
  }

  close(): void {
    this.#cursor.close();
  }
}
