import type { ReconciliationSnapshot } from "./reconciliation-snapshot.js";
import type {
  DataPublicationUpdates,
  DataSubscriptionUpdates,
} from "./types.js";
import {
  ErrorCode,
  PublicationAdmissionStatus,
  PublicationExecution,
  SubscriptionSourceStatus,
  type DataPublicationState,
  type DataSubscriptionState,
  type ErrorInfo,
} from "./schema.js";
import { OperationError } from "./agent-observation.js";
import {
  ResourceObservation,
  type NativeResourceObserver,
} from "./resource-observation.js";
import {
  ResourceOwner,
  ResourceOwners,
  type ResourceBinding,
  type ResourceRuntime,
} from "./resources.js";
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

interface DataTrack<D extends Delivery> {
  readonly kind: "data";
  readonly label: string;
  readonly delivery: D;
}
export interface LocalDataTrack<D extends Delivery = Delivery>
  extends DataTrack<D> {
  publish(): DataPublication;
}
export interface RemoteDataTrack<D extends Delivery = Delivery>
  extends DataTrack<D> {
  subscribe(): DataSubscription<D>;
  readonly room: string;
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

function failure(error: unknown): Error {
  if (error instanceof DataError || error instanceof OperationError)
    return error;
  if (typeof error === "object" && error !== null && "pulsebeamInfo" in error) {
    return new OperationError(error.pulsebeamInfo as ErrorInfo);
  }
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

function bytes(data: Uint8Array): void {
  if (!(data instanceof Uint8Array))
    throw new OperationError({
      code: ErrorCode.InvalidArgument,
      message: "Data send requires Uint8Array",
    });
}

export interface DataRuntime extends ResourceRuntime {
  send_owned_data(owner: bigint, payload: Uint8Array): Promise<void>;
  data_publication(owner: bigint): NativeResourceObserver;
  data_subscription(owner: bigint): NativeResourceObserver;
}
interface DataHost {
  getSnapshot(): Pick<ReconciliationSnapshot, "catalog" | "roomId">;
  subscribe(listener: () => void): () => void;
}
type Source = {
  readonly room: string;
  readonly participantId: string;
  readonly label: string;
};
type PendingRead<D extends Delivery> = {
  resolve(result: DataUpdate<D>): void;
  reject(error: unknown): void;
};

export class DataRegistry {
  #closed = false;
  #closing: Promise<void> | undefined;
  readonly #locals = new Map<string, LocalDataTrack>();
  readonly #remotes = new Map<string, RemoteDataTrack>();
  readonly #streams = new Set<DataStream<Delivery>>();
  readonly #publications = new Set<DataPublication>();
  readonly #owners = new Map<bigint, DataStream<Delivery>>();

  constructor(
    readonly host: DataHost,
    readonly resources: ResourceOwners,
  ) {}

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
        publish: () => {
          this.#requireOpen();
          const publication = new DataPublication(this.resources, spec, () =>
            this.#publications.delete(publication),
          );
          this.#publications.add(publication);
          return publication;
        },
      });
      this.#locals.set(key, handle);
    }
    return handle as LocalDataTrack<D>;
  }

  remote<D extends Delivery>(
    value: DataTrackSpec<D>,
    source: { readonly room: string; readonly participantId: string },
  ): RemoteDataTrack<D> {
    this.#requireOpen();
    const spec = track.data(value.label, value.delivery);
    const { room, participantId } = source;
    const key = JSON.stringify([room, participantId, trackKey(spec)]);
    let handle = this.#remotes.get(key);
    if (!handle) {
      const selector = Object.freeze({
        room,
        participantId,
        label: spec.label,
      });
      const id = () =>
        this.host
          .getSnapshot()
          .catalog.tracks.find(
            (entry) =>
              this.host.getSnapshot().roomId === room &&
              entry.participantId === participantId &&
              entry.kind === "data" &&
              entry.label === spec.label &&
              entry.delivery === spec.delivery,
          )?.id ?? null;
      handle = Object.freeze({
        kind: "data" as const,
        label: spec.label,
        delivery: spec.delivery,
        room,
        participantId,
        get id() {
          return id();
        },
        get active() {
          return id() !== null;
        },
        subscribe: () => this.subscribe(spec, selector),
        watch: (listener: () => void) => this.host.subscribe(listener),
      });
      this.#remotes.set(key, handle);
    }
    return handle as RemoteDataTrack<D>;
  }

  #stream<D extends Delivery>(
    spec: DataTrackSpec<D>,
    source: Source,
  ): DataStream<D> {
    this.#requireOpen();
    const stream = new DataStream(this, spec, source);
    this.#streams.add(stream);
    return stream;
  }
  subscribe<D extends Delivery>(
    spec: DataTrackSpec<D>,
    source: Source,
  ): DataSubscription<D> {
    return new DataSubscription(this.#stream(spec, source));
  }
  deactivate(): void {
    this.#owners.clear();
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
  claimed(stream: DataStream<Delivery>, binding: ResourceBinding): void {
    this.#owners.set(binding.owner, stream);
  }
  unclaimed(stream: DataStream<Delivery>, binding: ResourceBinding): void {
    if (this.#owners.get(binding.owner) === stream)
      this.#owners.delete(binding.owner);
  }
  forget(stream: DataStream<Delivery>): void {
    this.#streams.delete(stream);
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#closed = true;
    const closing = [...this.#streams].map((stream) => stream.close());
    closing.push(
      ...[...this.#publications].map((publication) => publication.close()),
    );
    this.#locals.clear();
    this.#remotes.clear();
    this.deactivate();
    return (this.#closing = Promise.all(closing).then(() => {}));
  }
  #requireOpen(): void {
    if (this.#closed)
      throw new OperationError({
        code: ErrorCode.Closed,
        message: "Agent is closed",
      });
  }
}

export class DataPublication {
  readonly #resource: ResourceOwner;
  readonly #observation = new ResourceObservation<DataPublicationState>();
  readonly #retired: () => void;
  readonly #pending = new Set<Promise<void>>();
  #closing: Promise<void> | undefined;

  constructor(
    resources: ResourceOwners,
    spec: DataTrackSpec,
    retired: () => void,
  ) {
    this.#retired = retired;
    this.#resource = resources.acquire(
      { op: "declareDataPublication", spec },
      {
        bind: (binding) =>
          this.#observation.bind(
            (binding.runtime as DataRuntime).data_publication(binding.owner),
          ),
        unbind: () => {},
      },
    );
  }
  state(): Readonly<DataPublicationState> {
    return this.#observation.state();
  }
  updates(): DataPublicationUpdates {
    return this.#observation.updates();
  }
  isClosed(): boolean {
    return this.#resource.isClosed();
  }
  send(data: Uint8Array): Promise<void> {
    try {
      bytes(data);
      if (this.isClosed())
        throw new OperationError({
          code: ErrorCode.Closed,
          message: "Data publication is closed",
        });
      const binding = this.#resource.binding;
      if (!binding || !this.#resource.current(binding))
        throw new OperationError({
          code: ErrorCode.NotReady,
          message: "Data runtime is not active",
        });
      const pending = (binding.runtime as DataRuntime)
        .send_owned_data(binding.owner, data)
        .catch((error) => {
          throw failure(error);
        });
      this.#pending.add(pending);
      void pending.then(
        () => this.#pending.delete(pending),
        () => this.#pending.delete(pending),
      );
      return pending;
    } catch (error) {
      return Promise.reject(failure(error));
    }
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    const release = this.#resource.close();
    return (this.#closing = Promise.allSettled([
      release,
      ...this.#pending,
    ]).then(() => {
      this.#retired();
    }));
  }
}

export interface DataReceiver<D extends Delivery = Delivery> {
  next(): Promise<DataUpdate<D>>;
  close(): Promise<void>;
  isClosed(): boolean;
}
export class DataSubscription<D extends Delivery = Delivery>
  implements DataReceiver<D>
{
  readonly #stream: DataStream<D>;
  constructor(stream: DataStream<D>) {
    this.#stream = stream;
  }
  state(): Readonly<DataSubscriptionState> {
    return this.#stream.state();
  }
  updates(): DataSubscriptionUpdates {
    return this.#stream.updates();
  }
  next(): Promise<DataUpdate<D>> {
    return this.#stream.next();
  }
  close(): Promise<void> {
    return this.#stream.close();
  }
  isClosed(): boolean {
    return this.#stream.isClosed();
  }
}

class DataStream<D extends Delivery> {
  #closed = false;
  #error: Error | undefined;
  readonly #resource: ResourceOwner;
  readonly #observation = new ResourceObservation<DataSubscriptionState>();
  #watcher: DataSubscriptionUpdates | undefined;
  #closing: Promise<void> | undefined;
  #pumping = false;
  #again = false;
  readonly #pending: PendingRead<D>[] = [];

  constructor(
    private readonly registry: DataRegistry,
    spec: DataTrackSpec<D>,
    source: Source,
  ) {
    let bound: ResourceBinding | undefined;
    this.#resource = registry.resources.acquire(
      { op: "receiveData", spec, source },
      {
        bind: (binding) => {
          bound = binding;
          this.#observation.bind(
            (binding.runtime as DataRuntime).data_subscription(binding.owner),
          );
          registry.claimed(this, binding);
          const watcher = this.#observation.updates();
          this.#watcher = watcher;
          void this.#watch(watcher).catch((error) => {
            if (this.#watcher === watcher) this.#fail(failure(error));
          });
        },
        unbind: () => {
          this.#watcher?.close();
          this.#watcher = undefined;
          if (bound) registry.unclaimed(this, bound);
          bound = undefined;
        },
        error: (error) => this.#fail(failure(error)),
      },
    );
    void this.#resource.ready.then(
      () => this.wake(),
      (error) => this.#fail(failure(error)),
    );
  }
  state(): Readonly<DataSubscriptionState> {
    return this.#observation.state();
  }
  updates(): DataSubscriptionUpdates {
    return this.#observation.updates();
  }
  isClosed(): boolean {
    return this.#closed;
  }
  #capture(state: Readonly<DataSubscriptionState>): void {
    const cause =
      state.terminalError ??
      (state.execution === PublicationExecution.Failed
        ? state.executionError
        : undefined) ??
      (state.sourceStatus === SubscriptionSourceStatus.Rejected ||
      state.admission === PublicationAdmissionStatus.Rejected
        ? state.error
        : undefined);
    if (cause && !this.#error) this.#error = new OperationError(cause);
  }
  async #watch(watcher: DataSubscriptionUpdates): Promise<void> {
    while (this.#watcher === watcher) {
      const state = await watcher.next();
      if (this.#watcher !== watcher || state === undefined) return;
      this.#capture(state);
      if (this.#error) {
        this.#fail(this.#error);
        return;
      }
      this.wake();
    }
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
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    if (this.#watcher) this.#capture(this.state());
    this.#closed = true;
    const release = this.#resource.close();
    for (const pending of this.#pending.splice(0)) {
      if (this.#error) pending.reject(this.#error);
      else pending.resolve({ type: "closed" });
    }
    return (this.#closing = release.then(() => {
      this.registry.forget(this);
    }));
  }
  #fail(error: Error): void {
    this.#error ??= error;
    void this.close();
  }
  wake(): void {
    this.#again = true;
    if (!this.#pumping) void this.#drain();
  }
  async #drain(): Promise<void> {
    this.#pumping = true;
    try {
      while (!this.#closed && this.#pending.length && this.#resource.binding) {
        const binding = this.#resource.binding;
        this.#again = false;
        let value: unknown;
        try {
          value = await binding.runtime.resource({
            op: "readData",
            owner: binding.owner,
          });
        } catch (error) {
          if (!this.#closed && this.#resource.current(binding))
            this.#fail(failure(error));
          continue;
        }
        if (this.#closed || !this.#resource.current(binding)) continue;
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
