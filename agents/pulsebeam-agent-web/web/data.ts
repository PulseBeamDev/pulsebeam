import type { AgentSnapshot } from "./types.js";
import { track, trackKey, type DataTrackSpec, type Delivery } from "./track.js";

export interface DataMessage {
  readonly type: "message";
  readonly participantId: string;
  readonly trackId: string;
  readonly sequence: bigint;
  readonly data: Uint8Array;
}

export interface DataGap {
  readonly type: "gap";
  readonly participantId: string;
  readonly trackId: string;
  readonly from: bigint;
  readonly to: bigint;
}

export type DataEvent<D extends Delivery = Delivery> =
  | DataMessage
  | (D extends "ordered" ? DataGap : never);
export type DataUpdate<D extends Delivery = Delivery> =
  | { readonly type: "event"; readonly event: DataEvent<D> }
  | { readonly type: "closed" };

interface DataReceive<D extends Delivery> {
  readonly kind: "data";
  readonly label: string;
  readonly delivery: D;
  receive(): Promise<DataReceiver<D>>;
}

export interface LocalDataTrack<D extends Delivery = Delivery>
  extends DataReceive<D> {
  send(data: Uint8Array): Promise<void>;
}

export interface RemoteDataTrack<D extends Delivery = Delivery>
  extends DataReceive<D> {
  readonly participantId: string;
  readonly id: string | null;
  readonly active: boolean;
  watch(listener: () => void): () => void;
}

export class DataError extends Error {
  constructor(
    readonly code: string,
    message: string,
    cause?: unknown,
  ) {
    super(message, { cause });
    this.name = "DataError";
  }
}

function failure(error: unknown): DataError {
  if (error instanceof DataError) return error;
  const code =
    typeof error === "object" &&
    error !== null &&
    "pulsebeamCode" in error &&
    typeof error.pulsebeamCode === "string"
      ? error.pulsebeamCode
      : "runtime";
  return new DataError(
    code,
    error instanceof Error ? error.message : String(error),
    error,
  );
}

export interface DataRuntime {
  resource(command: unknown): Promise<unknown>;
  send_data(spec: unknown, payload: Uint8Array): Promise<void>;
}

interface DataHost {
  getSnapshot(): Pick<AgentSnapshot, "catalog">;
  subscribe(listener: () => void): () => void;
}

interface Binding {
  readonly runtime: DataRuntime;
  readonly owner: bigint;
}
type PendingRead<D extends Delivery> = {
  resolve(result: DataUpdate<D>): void;
  reject(error: unknown): void;
};

export class DataRegistry {
  #runtime: DataRuntime | undefined;
  #closed = false;
  #closing: Promise<void> | undefined;
  readonly #locals = new Map<string, LocalDataTrack>();
  readonly #remotes = new Map<string, RemoteDataTrack>();
  readonly #streams = new Set<DataStream<Delivery>>();
  readonly #owners = new Map<bigint, DataStream<Delivery>>();

  constructor(readonly host: DataHost) {}

  local<D extends Delivery>(value: DataTrackSpec<D>): LocalDataTrack<D> {
    this.#requireOpen();
    const spec = track.data(value.label, value.delivery);
    const key = trackKey(spec);
    let handle = this.#locals.get(key);
    if (!handle) {
      handle = Object.freeze({
        kind: "data" as const,
        label: spec.label,
        delivery: spec.delivery,
        send: (data: Uint8Array) => this.#send(spec, data),
        receive: () => this.receive(spec),
      });
      this.#locals.set(key, handle);
    }
    return handle as LocalDataTrack<D>;
  }

  remote<D extends Delivery>(
    value: DataTrackSpec<D>,
    participantId: string,
  ): RemoteDataTrack<D> {
    this.#requireOpen();
    const spec = track.data(value.label, value.delivery);
    const key = JSON.stringify([participantId, trackKey(spec)]);
    let handle = this.#remotes.get(key);
    if (!handle) {
      const registry = this;
      const id = () =>
        this.host
          .getSnapshot()
          .catalog.tracks.find(
            (entry) =>
              entry.participantId === participantId &&
              entry.kind === "data" &&
              entry.label === spec.label &&
              entry.delivery === spec.delivery,
          )?.id ?? null;
      handle = Object.freeze({
        kind: "data" as const,
        label: spec.label,
        delivery: spec.delivery,
        participantId,
        get id() {
          return id();
        },
        get active() {
          return id() !== null;
        },
        receive: () => registry.receive(spec, participantId),
        watch: (listener: () => void) => registry.host.subscribe(listener),
      });
      this.#remotes.set(key, handle);
    }
    return handle as RemoteDataTrack<D>;
  }

  async receive<D extends Delivery>(
    spec: DataTrackSpec<D>,
    participantId?: string,
  ): Promise<DataReceiver<D>> {
    this.#requireOpen();
    const stream = new DataStream(this, spec, participantId);
    this.#streams.add(stream);
    if (this.#runtime) stream.bind(this.#runtime);
    await stream.ready();
    return new DataReceiver(stream);
  }

  activate(runtime: DataRuntime): void {
    if (this.#closed || this.#runtime === runtime) return;
    this.deactivate();
    this.#runtime = runtime;
    for (const stream of this.#streams) stream.bind(runtime);
  }

  deactivate(): void {
    this.#runtime = undefined;
    this.#owners.clear();
    for (const stream of this.#streams) stream.detach();
  }

  notify(value: unknown): boolean {
    if (
      typeof value !== "object" ||
      value === null ||
      !("type" in value) ||
      value.type !== "data-ready"
    )
      return false;
    if ("owners" in value && Array.isArray(value.owners))
      for (const owner of value.owners) this.#owners.get(owner)?.wake();
    return true;
  }

  current(runtime: DataRuntime): boolean {
    return this.#runtime === runtime && !this.#closed;
  }
  claimed(stream: DataStream<Delivery>, binding: Binding): void {
    this.#owners.set(binding.owner, stream);
  }
  forget(stream: DataStream<Delivery>): void {
    this.#streams.delete(stream);
  }
  release(stream: DataStream<Delivery>, binding: Binding): Promise<void> {
    if (this.#owners.get(binding.owner) === stream)
      this.#owners.delete(binding.owner);
    return release(binding);
  }

  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#closed = true;
    const closing = [...this.#streams].map((stream) => stream.close());
    this.#locals.clear();
    this.#remotes.clear();
    this.deactivate();
    return (this.#closing = Promise.all(closing).then(() => {}));
  }

  #requireOpen(): void {
    if (this.#closed) throw new DataError("closed", "agent is closed");
  }
  #send(spec: DataTrackSpec, data: Uint8Array): Promise<void> {
    try {
      this.#requireOpen();
      if (!(data instanceof Uint8Array))
        throw new TypeError("data send requires Uint8Array");
      if (!this.#runtime)
        throw new DataError("unavailable", "agent runtime is not active");
      return this.#runtime.send_data(spec, data).catch((error) => {
        throw failure(error);
      });
    } catch (error) {
      return Promise.reject(failure(error));
    }
  }
}

async function release(binding: Binding): Promise<void> {
  try {
    await binding.runtime.resource({ op: "release", owner: binding.owner });
  } catch {
    /* A retired runtime already owns teardown of its resources. */
  }
}

export class DataReceiver<D extends Delivery = Delivery> {
  constructor(private readonly stream: DataStream<D>) {}

  next(): Promise<DataUpdate<D>> {
    return this.stream.next();
  }
  close(): Promise<void> {
    return this.stream.close();
  }
  isClosed(): boolean {
    return this.stream.isClosed();
  }
}

class DataStream<D extends Delivery> {
  #closed = false;
  #error: DataError | undefined;
  #binding: Binding | undefined;
  #attachment: object | undefined;
  readonly #lifecycle = new Set<Promise<void>>();
  #closing: Promise<void> | undefined;
  #pumping = false;
  #again = false;
  readonly #pending: PendingRead<D>[] = [];
  #resolveReady!: () => void;
  #rejectReady!: (error: DataError) => void;
  readonly #ready = new Promise<void>((resolve, reject) => {
    this.#resolveReady = resolve;
    this.#rejectReady = reject;
  });

  constructor(
    private registry: DataRegistry,
    private spec: DataTrackSpec<D>,
    private participantId?: string,
  ) {}

  ready(): Promise<void> {
    return this.#ready;
  }
  isClosed(): boolean {
    return this.#closed;
  }
  next(): Promise<DataUpdate<D>> {
    if (this.#error) return Promise.reject(this.#error);
    if (this.#closed) return Promise.resolve({ type: "closed" });
    const result = new Promise<DataUpdate<D>>((resolve, reject) =>
      this.#pending.push({ resolve, reject }),
    );
    this.wake();
    return result;
  }
  bind(runtime: DataRuntime): void {
    if (this.#closed) return;
    this.detach();
    const attachment = {};
    this.#attachment = attachment;
    this.#track(this.#acquire(runtime, attachment));
  }
  detach(): void {
    this.#attachment = undefined;
    const binding = this.#binding;
    this.#binding = undefined;
    if (binding) this.#track(this.registry.release(this, binding));
  }
  #track(promise: Promise<void>): void {
    this.#lifecycle.add(promise);
    void promise.then(
      () => this.#lifecycle.delete(promise),
      () => this.#lifecycle.delete(promise),
    );
  }
  async #acquire(runtime: DataRuntime, attachment: object): Promise<void> {
    try {
      const value = await runtime.resource({
        op: "receiveData",
        spec: this.spec,
        participantId: this.participantId,
      });
      if (
        typeof value !== "object" ||
        value === null ||
        !("owner" in value) ||
        typeof value.owner !== "bigint"
      )
        throw new DataError("runtime", "invalid data receive owner");
      const binding = { runtime, owner: value.owner };
      if (
        this.#closed ||
        this.#attachment !== attachment ||
        !this.registry.current(runtime)
      ) {
        await release(binding);
        return;
      }
      this.#binding = binding;
      this.registry.claimed(this, binding);
      this.#resolveReady();
      this.wake();
    } catch (error) {
      if (
        !this.#closed &&
        this.#attachment === attachment &&
        this.registry.current(runtime)
      )
        this.#fail(failure(error));
    }
  }
  close(clearError = true): Promise<void> {
    if (clearError) this.#error = undefined;
    if (this.#closed) return this.#closing ?? Promise.resolve();
    this.#closed = true;
    this.#resolveReady();
    this.#attachment = undefined;
    const binding = this.#binding;
    this.#binding = undefined;
    for (const pending of this.#pending.splice(0)) {
      if (this.#error) pending.reject(this.#error);
      else pending.resolve({ type: "closed" });
    }
    this.#closing = Promise.all([
      binding ? this.registry.release(this, binding) : undefined,
      ...this.#lifecycle,
    ]).then(() => this.registry.forget(this));
    return this.#closing;
  }
  #fail(error: DataError): void {
    this.#error = error;
    this.#rejectReady(error);
    void this.close(false);
  }
  wake(): void {
    this.#again = true;
    if (!this.#pumping) void this.#drain();
  }
  async #drain(): Promise<void> {
    this.#pumping = true;
    try {
      while (!this.#closed && this.#pending.length && this.#binding) {
        const binding = this.#binding;
        this.#again = false;
        let value: unknown;
        try {
          value = await binding.runtime.resource({
            op: "readData",
            owner: binding.owner,
          });
        } catch (error) {
          if (!this.#closed && this.#binding === binding)
            this.#fail(failure(error));
          continue;
        }
        if (this.#closed || this.#binding !== binding) continue;
        if (value === null) {
          if (this.#again) continue;
          break;
        }
        this.#pending.shift()?.resolve({
          type: "event",
          event: Object.freeze(value) as DataEvent<D>,
        });
      }
    } finally {
      this.#pumping = false;
    }
  }
}
