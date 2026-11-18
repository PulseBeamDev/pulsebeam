import type { BrowserRuntime } from "./wasm.js";
import type { AgentState, ErrorInfo } from "./schema.js";
import type { AgentUpdates } from "./types.js";

type Runtime = InstanceType<typeof BrowserRuntime>;
type Observer = ReturnType<Runtime["observer"]>;
type Cursor = ReturnType<Observer["updates"]>;

export class OperationError extends Error {
  readonly info: Readonly<ErrorInfo>;

  constructor(info: ErrorInfo) {
    super(info.message);
    this.name = "OperationError";
    this.info = Object.freeze({ ...info });
  }
}

function raise(error: unknown): never {
  if (
    typeof error === "object" &&
    error !== null &&
    "pulsebeamInfo" in error
  ) {
    throw new OperationError(error.pulsebeamInfo as ErrorInfo);
  }
  throw error;
}

function immutable(state: AgentState): Readonly<AgentState> {
  return Object.freeze({
    ...state,
    error: state.error ? Object.freeze({ ...state.error }) : undefined,
  });
}

export class AgentObservation {
  readonly #observer: Observer;
  #runtime: Runtime | undefined;
  #value: Readonly<AgentState> | undefined;

  constructor(runtime: Runtime) {
    this.#runtime = runtime;
    this.#observer = runtime.observer();
  }

  state(): Readonly<AgentState> {
    return this.accept(
      (this.#runtime?.state() ?? this.#observer.state()) as AgentState,
    );
  }

  accept(value: AgentState): Readonly<AgentState> {
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

  updates(): AgentUpdates {
    this.state();
    try {
      return new AgentUpdatesFacade(this.#observer.updates(), this);
    } catch (error) {
      return raise(error);
    }
  }

  retire(): void {
    this.#runtime = undefined;
  }
}

class AgentUpdatesFacade implements AgentUpdates {
  readonly #cursor: Cursor;
  readonly #observation: AgentObservation;

  constructor(cursor: Cursor, observation: AgentObservation) {
    this.#cursor = cursor;
    this.#observation = observation;
  }

  async next(): Promise<Readonly<AgentState> | undefined> {
    try {
      const value = (await this.#cursor.next()) as AgentState | undefined;
      return value === undefined ? undefined : this.#observation.accept(value);
    } catch (error) {
      return raise(error);
    }
  }

  close(): void {
    this.#cursor.close();
  }
}
