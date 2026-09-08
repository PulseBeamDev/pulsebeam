import type {
  Agent,
  AgentSnapshot,
  AgentState,
  LocalTrack,
  TrackRef,
} from "./types.js";
import { afterInitialization } from "./wasm.js";

const EMPTY_TRACKS: readonly [] = Object.freeze([]);

function snapshot(connection: AgentSnapshot["connection"]): AgentSnapshot {
  return Object.freeze({
    connection,
    participantId: null,
    tracks: EMPTY_TRACKS,
  });
}

function copyState(state: AgentState): AgentState {
  return Object.freeze({
    connection:
      state.connection === null ? null : Object.freeze({ ...state.connection }),
    publish: Object.freeze([...(state.publish ?? [])]) as readonly LocalTrack[],
    subscribe: Object.freeze([
      ...(state.subscribe ?? []),
    ]) as readonly TrackRef[],
  });
}

class AgentFacade implements Agent {
  #snapshot: AgentSnapshot = snapshot("disconnected");
  #listeners = new Set<() => void>();
  #state: AgentState = Object.freeze({
    connection: null,
    publish: EMPTY_TRACKS,
    subscribe: EMPTY_TRACKS,
  });
  #revision = 0;
  #closed = false;

  readonly getSnapshot = (): AgentSnapshot => this.#snapshot;

  readonly subscribe = (listener: () => void): (() => void) => {
    if (this.#closed) return () => {};
    this.#listeners.add(listener);
    let subscribed = true;
    return () => {
      if (!subscribed) return;
      subscribed = false;
      this.#listeners.delete(listener);
    };
  };

  setState(state: AgentState): void {
    if (this.#closed) return;
    this.#state = copyState(state);
    const revision = ++this.#revision;
    if (this.#state.connection === null) {
      this.#publish("disconnected");
      return;
    }
    this.#publish("connecting");
    afterInitialization(() => this.#fail(revision));
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    ++this.#revision;
    this.#state = Object.freeze({
      connection: null,
      publish: EMPTY_TRACKS,
      subscribe: EMPTY_TRACKS,
    });
    this.#publish("disconnected");
    this.#listeners.clear();
  }

  #fail(revision: number): void {
    if (
      this.#closed ||
      revision !== this.#revision ||
      this.#state.connection === null
    )
      return;
    this.#publish("failed");
  }

  #publish(connection: AgentSnapshot["connection"]): void {
    if (this.#snapshot.connection === connection) return;
    this.#snapshot = snapshot(connection);
    for (const listener of [...this.#listeners]) listener();
  }
}

export function createAgent(): Agent {
  return new AgentFacade();
}
