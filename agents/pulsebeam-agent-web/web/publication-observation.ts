import type { BrowserRuntime } from "./wasm.js";
import type { PublicationState } from "./schema.js";
import type { PublicationUpdates } from "./types.js";
import { raise } from "./agent-observation.js";

type Runtime = InstanceType<typeof BrowserRuntime>;
type Observer = ReturnType<Runtime["publication"]>;
type Cursor = ReturnType<Observer["updates"]>;

function immutable(state: PublicationState): Readonly<PublicationState> {
  return Object.freeze({
    ...state,
    configuration: Object.freeze({
      ...state.configuration,
      error: state.configuration.error
        ? Object.freeze({ ...state.configuration.error })
        : undefined,
    }),
    admission: Object.freeze({
      ...state.admission,
      error: state.admission.error
        ? Object.freeze({ ...state.admission.error })
        : undefined,
      executionError: state.admission.executionError
        ? Object.freeze({ ...state.admission.executionError })
        : undefined,
    }),
  });
}

export class PublicationObservation {
  #current: BoundPublicationObservation | undefined;

  bind(runtime: Runtime, owner: bigint): void {
    this.#current = new BoundPublicationObservation(runtime.publication(owner));
  }

  #bound(): BoundPublicationObservation {
    if (!this.#current) throw new Error("publication state is not ready");
    return this.#current;
  }

  state(): Readonly<PublicationState> {
    return this.#bound().state();
  }

  updates(): PublicationUpdates {
    return this.#bound().updates();
  }
}

class BoundPublicationObservation {
  readonly #observer: Observer;
  #value: Readonly<PublicationState> | undefined;

  constructor(observer: Observer) {
    this.#observer = observer;
  }

  state(): Readonly<PublicationState> {
    return this.accept(this.#observer.state() as PublicationState);
  }

  accept(value: PublicationState): Readonly<PublicationState> {
    if (
      this.#value &&
      value.observationRevision === this.#value.observationRevision
    )
      return this.#value;
    const next = immutable(value);
    if (
      !this.#value ||
      value.observationRevision > this.#value.observationRevision
    )
      this.#value = next;
    return next;
  }

  updates(): PublicationUpdates {
    this.state();
    try {
      return new PublicationUpdatesFacade(this.#observer.updates(), this);
    } catch (error) {
      return raise(error);
    }
  }
}

class PublicationUpdatesFacade implements PublicationUpdates {
  readonly #cursor: Cursor;
  readonly #observation: BoundPublicationObservation;

  constructor(cursor: Cursor, observation: BoundPublicationObservation) {
    this.#cursor = cursor;
    this.#observation = observation;
  }

  async next(): Promise<Readonly<PublicationState> | undefined> {
    try {
      const value = (await this.#cursor.next()) as PublicationState | undefined;
      return value === undefined ? undefined : this.#observation.accept(value);
    } catch (error) {
      return raise(error);
    }
  }

  close(): void {
    this.#cursor.close();
  }
}
