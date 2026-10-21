import {
  nativeCaptureTrack,
  type CapturedAudioTrack,
  type CapturedTrack,
  type CapturedVideoTrack,
} from "./capture-source.js";
import { DataRegistry, type LocalDataTrack } from "./data.js";
import type {
  AudioTrackSpec,
  DataTrackSpec,
  Delivery,
  VideoTrackSpec,
} from "./track.js";
import type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentError,
  AgentSnapshot,
  AgentState,
  CatalogSnapshot,
  ConnectionState,
  LogLevel,
  LocalAudioTrack,
  LocalVideoTrack,
  MediaTopology,
  MappingSnapshot,
  Participant,
  Publication,
  PublicationIntent,
  RemoteTrack,
  LocalMedia,
  RemoteMedia,
  SenderConfig,
  Topic,
  TopicMode,
  TopicRegistration,
  TopicSnapshot,
  TopicCallbacks,
  TopicPublisherStatus,
} from "./types.js";
import { RemoteCatalog } from "./remote-catalog.js";
import { TopicRegistry } from "./topic.js";
import { ResourceOwners } from "./resources.js";
import { LocalMediaRegistry } from "./media.js";
import { BrowserRuntime, whenInitialized } from "./wasm.js";
import {
  copyConfig,
  copyState,
  desiredValue,
  emptySnapshot,
  freezeTopicSnapshot,
  validatePlayoutDelay,
  type RuntimeConfig,
} from "./agent-state.js";
import { LocalTrackHandle, LocalTrackCapacityError } from "./local-track.js";
export { LocalTrackCapacityError } from "./local-track.js";

type Runtime = InstanceType<typeof BrowserRuntime>;

type RuntimeTopicCounter =
  | "acceptedSends"
  | "droppedSends"
  | "deliveredMessages"
  | "channelFailures";

interface RuntimeSnapshot {
  readonly version: bigint;
  readonly desiredRevision: bigint;
  readonly connection: ConnectionState;
  readonly generation?: bigint;
  readonly participantId?: string;
  readonly roomId?: string;
  readonly authorizationExpiresAt?: bigint;
  readonly catalog: Omit<CatalogSnapshot, "revision"> & {
    readonly revision: bigint;
  };
  readonly mapping: Omit<MappingSnapshot, "acceptedIntentRevision"> & {
    readonly acceptedIntentRevision: bigint;
  };
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly topics: Omit<TopicSnapshot, RuntimeTopicCounter | "publishers"> &
    Record<RuntimeTopicCounter, bigint> & {
      readonly publishers: readonly (Omit<
        TopicPublisherStatus,
        "dataTrackId"
      > & { readonly dataTrackId?: string })[];
    };
  readonly failure?: AgentError;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function localFailureClass(error: unknown): AgentError["class"] {
  if (
    typeof error === "object" &&
    error !== null &&
    "pulsebeamClass" in error &&
    (error.pulsebeamClass === "validation" ||
      error.pulsebeamClass === "protocol")
  ) {
    return error.pulsebeamClass;
  }
  return "runtime";
}

class AgentFacade implements Agent {
  #snapshot: AgentSnapshot = emptySnapshot("disconnected");
  #listeners = new Set<() => void>();
  #eventListeners = new Set<(event: AgentEvent) => void>();
  #state: AgentState = copyState({ connected: false });
  #runtime: Runtime | undefined;
  #runtimeGeneration: bigint | null = null;
  #localSourcePresent = new Map<string, boolean>();
  #localMuted = new Map<string, boolean>();
  #localVideoHandles = new Map<
    string,
    LocalTrackHandle<"video", CapturedVideoTrack>
  >();
  #localAudioHandles = new Map<
    string,
    LocalTrackHandle<"audio", CapturedAudioTrack>
  >();
  #reservedSlots = new Map<string, string>();
  #localPublications = new Map<string, PublicationIntent>();
  #relationships = new Set<object>();
  #publicationOwners = new Map<
    string,
    { owner: object; handle: LocalVideoTrack | LocalAudioTrack; slot: string }
  >();
  #localConfigs = new Map<string, SenderConfig>();
  #jsxMutation = false;
  #localOperations = new Map<string, number>();
  readonly #data = new DataRegistry(this);
  readonly #resources = new ResourceOwners((error) => {
    if (
      this.#runtimeConfig.logLevel !== "off" &&
      this.#runtimeConfig.logLevel !== "error"
    )
      console.warn("PulseBeam resource ownership failed", error);
  });
  readonly #media = new LocalMediaRegistry(this.#resources);
  readonly #remote: RemoteCatalog;
  readonly #topics: TopicRegistry;
  #managedTopics: readonly TopicRegistration[] = [];
  readonly #topology: Required<MediaTopology>;
  #closed = false;
  #active = false;
  #activation = 0;
  #ready: Promise<Runtime> | undefined;
  #runtimeConfig: RuntimeConfig;

  constructor(config: AgentConfig, managed = false) {
    const runtimeConfig = copyConfig(config);
    this.#runtimeConfig = runtimeConfig;
    this.#topology = runtimeConfig.topology;
    this.#remote = new RemoteCatalog(
      this,
      this.#data,
      this.#resources,
      runtimeConfig.logLevel,
    );
    this.#topics = new TopicRegistry(
      this,
      (name, mode, payload) => this.#publishTopic(name, mode, payload),
      (topics) => {
        const previous = this.#managedTopics;
        this.#managedTopics = topics;
        try {
          this.#applyState(this.#state);
        } catch (error) {
          this.#managedTopics = previous;
          throw error;
        }
      },
    );
    if (!managed) this.activate();
  }

  // React can create an inert handle during render. Activation only happens
  // after React commits, and can be repeated after Strict Mode effect cleanup.
  activate(): void {
    if (this.#closed) throw new Error("agent is closed");
    if (this.#active) return;
    this.#active = true;
    const activation = ++this.#activation;
    const active = () =>
      !this.#closed && this.#active && activation === this.#activation;
    this.#ready = whenInitialized()
      .catch((error: unknown) => {
        if (active()) this.#terminalFailure("initialization", message(error));
        throw error;
      })
      .then(() => {
        if (!active()) throw new Error("agent is inactive");
        let runtime: Runtime;
        try {
          runtime = new BrowserRuntime(this.#runtimeConfig);
        } catch (error) {
          if (active())
            this.#terminalFailure("invalid-configuration", message(error));
          throw error;
        }
        this.#runtime = runtime;
        this.#data.activate(runtime);
        this.#resources.activate(runtime);
        runtime.set_event_listener((event: unknown) => {
          if (active()) this.#runtimeEvent(event);
        });
        runtime.set_error_listener((error: unknown) => {
          if (active())
            this.#emitFailure(localFailureClass(error), message(error));
        });
        try {
          runtime.replace_desired(
            desiredValue(this.#effectiveState(this.#state)),
          );
        } catch (error) {
          if (active())
            this.#terminalFailure("invalid-configuration", message(error));
          this.#data.deactivate();
          this.#resources.deactivate();
          runtime.abort();
          this.#runtime = undefined;
          this.#runtimeGeneration = null;
          throw error;
        }
        runtime.set_snapshot_listener((snapshot: RuntimeSnapshot) => {
          if (active()) this.#runtimeSnapshot(runtime, snapshot);
        });
        // A Strict Mode cleanup may have interrupted local-track operations.
        // Reapply the currently desired captured sources to each fresh runtime.
        for (const [slot, label] of this.#reservedSlots) {
          const video = slot.startsWith("v");
          const source = video
            ? this.#localVideoHandles.get(label)?.source
            : this.#localAudioHandles.get(label)?.source;
          if (!source) continue;
          this.#jsxMutation = true;
          try {
            void this.replaceLocalTrack(
              slot,
              nativeCaptureTrack(source, video ? "video" : "audio"),
              this.#localConfigs.get(slot) ??
                (video ? { contentHint: "motion" } : { contentHint: "speech" }),
            ).catch(() => {});
          } finally {
            this.#jsxMutation = false;
          }
        }
        return runtime;
      });
    void this.#ready.catch(() => {});
  }

  deactivate(): void {
    if (!this.#active || this.#closed) return;
    this.#active = false;
    ++this.#activation;
    this.#ready = undefined;
    const runtime = this.#runtime;
    this.#runtime = undefined;
    this.#runtimeGeneration = null;
    this.#data.deactivate();
    this.#resources.deactivate();
    if (runtime) {
      runtime.set_snapshot_listener();
      runtime.set_event_listener();
      runtime.set_error_listener();
      runtime.close();
      runtime.abort();
    }
    this.#remote.resetPlayback();
    this.#publish(emptySnapshot("disconnected"));
  }

  readonly local: LocalMedia = Object.freeze({
    video: (label: string) => this.#localVideoTrack(label),
    audio: (label: string) => this.#localAudioTrack(label),
  });

  get connection(): ConnectionState {
    return this.#snapshot.connection;
  }

  get error(): AgentError | null {
    return this.#snapshot.failure;
  }

  get participantId(): string | null {
    return this.#snapshot.participantId;
  }

  get roomId(): string | null {
    return this.#snapshot.roomId;
  }

  get authorizationExpiresAt(): number | null {
    return this.#snapshot.authorizationExpiresAt;
  }

  get remote(): RemoteMedia {
    return this.#remote.facade();
  }

  bindPublicAgent(agent: Agent): void {
    this.#remote.bindAgent(agent);
  }

  track(spec: VideoTrackSpec): LocalVideoTrack;
  track(spec: AudioTrackSpec): LocalAudioTrack;
  track<D extends Delivery>(spec: DataTrackSpec<D>): LocalDataTrack<D>;
  track(
    spec: VideoTrackSpec | AudioTrackSpec | DataTrackSpec,
  ): LocalVideoTrack | LocalAudioTrack | LocalDataTrack {
    if (spec.kind === "data") return this.#data.local(spec);
    if (spec.kind === "video") return this.#media.track(spec);
    return this.#media.track(spec);
  }

  topic<T>(name: string, options: { readonly mode: TopicMode }): Topic<T> {
    return this.#topics.topic(name, options);
  }

  listenTopic<T>(
    name: string,
    options: { readonly mode: TopicMode },
    callbacks: TopicCallbacks<T>,
  ): () => void {
    return this.#topics.listen(name, options, callbacks);
  }

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

  readonly subscribeEvents = (
    listener: (event: AgentEvent) => void,
  ): (() => void) => {
    if (this.#closed) return () => {};
    this.#eventListeners.add(listener);
    let subscribed = true;
    return () => {
      if (!subscribed) return;
      subscribed = false;
      this.#eventListeners.delete(listener);
    };
  };

  #localVideoTrack(label: string): LocalVideoTrack {
    return this.#localHandle(
      "video",
      label,
      this.#localVideoHandles,
      this.#topology.localVideos,
    );
  }

  #localAudioTrack(label: string): LocalAudioTrack {
    return this.#localHandle(
      "audio",
      label,
      this.#localAudioHandles,
      this.#topology.localAudios,
    );
  }

  #localHandle<K extends "video" | "audio", S extends CapturedTrack>(
    kind: K,
    label: string,
    handles: Map<string, LocalTrackHandle<K, S>>,
    capacity: number,
  ): LocalTrackHandle<K, S> {
    if (this.#closed) throw new Error("agent is closed");
    if (
      typeof label !== "string" ||
      !label ||
      new TextEncoder().encode(label).length > 64
    )
      throw new TypeError("invalid local track label");
    const existing = handles.get(label);
    if (existing) return existing;
    if (!Number.isSafeInteger(capacity) || handles.size >= capacity) {
      throw new LocalTrackCapacityError(kind, label, capacity);
    }
    const slot = `${kind === "video" ? "v" : "a"}${handles.size}`;
    if (
      this.#state.publications?.some((publication) => publication.slot === slot)
    ) {
      throw new TypeError(`reserved ${kind} slot ${slot} is already in use`);
    }
    const handle = new LocalTrackHandle<K, S>(
      kind,
      label,
      (source) => {
        this.#assertSlotWritable(slot);
        this.#setLocalSource(kind, slot, label, source);
      },
      (enabled) => {
        this.#assertSlotWritable(slot);
        void this.setLocalMuted(slot, !enabled).catch(() => {});
      },
    );
    handles.set(label, handle);
    this.#reservedSlots.set(slot, label);
    return handle;
  }

  #setLocalSource(
    kind: "video" | "audio",
    slot: string,
    label: string,
    source: CapturedTrack | null,
  ): void {
    if (this.#closed) throw new Error("agent is closed");
    const candidate = source === null ? null : nativeCaptureTrack(source, kind);
    const track = candidate?.readyState === "live" ? candidate : null;
    if (track) this.#localPublications.set(slot, { slot, label, active: true });
    else this.#localPublications.delete(slot);
    this.#applyState(this.#state);
    if (this.#active)
      void this.replaceLocalTrack(
        slot,
        track,
        this.#localConfigs.get(slot) ??
          (kind === "video"
            ? { contentHint: "motion" }
            : { contentHint: "speech" }),
      ).catch(() => {});
  }

  connect(): void {
    if (this.#closed) throw new Error("agent is closed");
    if (this.#snapshot.connection === "terminal-failure" && !this.#runtime) {
      this.deactivate();
      this.activate();
    }
    if (!this.#state.connected)
      this.#applyState({ ...this.#state, connected: true });
    else this.#runtime?.connect();
  }

  disconnect(): void {
    if (this.#closed) throw new Error("agent is closed");
    if (this.#state.connected)
      this.#applyState({ ...this.#state, connected: false });
  }

  setState(state: AgentState): void {
    if (this.#closed) return;
    if (this.#relationships.size)
      throw new RelationshipOwnershipError(
        "desired state is owned by committed JSX",
      );
    validatePlayoutDelay(state);
    for (const publication of state.publications ?? []) {
      const reserved = this.#reservedSlots.get(publication.slot);
      if (reserved !== undefined && reserved !== publication.label) {
        throw new TypeError(
          `slot ${publication.slot} is reserved for ${reserved}`,
        );
      }
    }
    this.#applyState(copyState(state));
  }

  #effectiveState(state: AgentState): AgentState {
    const topics = new Map<string, TopicRegistration>();
    for (const topic of state.topics ?? [])
      topics.set(`${topic.mode}:${topic.name}`, topic);
    for (const topic of this.#managedTopics) {
      const key = `${topic.mode}:${topic.name}`;
      const prior = topics.get(key);
      topics.set(key, {
        ...prior,
        ...topic,
        publish: !!(prior?.publish || topic.publish),
        subscribe: !!(prior?.subscribe || topic.subscribe),
      });
    }
    return {
      ...state,
      publications: [
        ...(state.publications ?? []).filter(
          ({ slot }) => !this.#reservedSlots.has(slot),
        ),
        ...this.#localPublications.values(),
      ],
      video: state.video ?? [],
      topics: [...topics.values()],
    };
  }

  #applyState(state: AgentState): void {
    if (this.#closed) return;
    const next = copyState(state);
    if (this.#runtime)
      this.#runtime.replace_desired(desiredValue(this.#effectiveState(next)));
    this.#state = next;
    if (!next.connected)
      this.#remote.update({ ...this.#snapshot, connection: "disconnected" });
    if (
      this.#active &&
      !this.#runtime &&
      this.#snapshot.connection !== "terminal-failure"
    ) {
      this.#publish(
        Object.freeze({
          ...this.#snapshot,
          connection: this.#state.connected ? "initializing" : "disconnected",
          failure: null,
        }),
      );
    }
  }

  replaceLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    config: SenderConfig,
  ): Promise<void> {
    if (this.#closed) return Promise.reject(new Error("agent is closed"));
    this.#assertSlotWritable(slot);
    const operation = (this.#localOperations.get(slot) ?? 0) + 1;
    this.#localOperations.set(slot, operation);
    const sender = Object.freeze({
      contentHint: config.contentHint,
      degradationPreference: config.degradationPreference,
      encodings: Object.freeze(
        (config.encodings ?? []).map((encoding) =>
          Object.freeze({ ...encoding }),
        ),
      ),
    });
    this.#localSourcePresent.set(slot, track !== null);
    return this.#runLocal(async (runtime) => {
      if (this.#localOperations.get(slot) !== operation) return;
      await runtime.set_local_muted(slot, this.#effectiveLocalMuted(slot));
      if (this.#localOperations.get(slot) !== operation) return;
      await runtime.replace_local_track(slot, track, sender);
      this.#requireCurrentRuntime(runtime);
    });
  }

  setLocalMuted(slot: string, muted: boolean): Promise<void> {
    if (this.#closed) return Promise.reject(new Error("agent is closed"));
    this.#assertSlotWritable(slot);
    this.#localMuted.set(slot, muted);
    return this.#runLocal(async (runtime) => {
      await runtime.set_local_muted(slot, this.#effectiveLocalMuted(slot));
    });
  }

  reconnect(): void {
    this.#requireRuntime().force_reconnect();
  }

  renewAuthorization(token: string): void {
    if (this.#closed) throw new Error("agent is closed");
    if (!token || token.length > 16_384 || !/^[!-~]+$/.test(token)) {
      throw new Error("authorization token is invalid");
    }
    if (token === this.#runtimeConfig.token) return;
    this.#runtime?.renew_authorization(token);
    this.#runtimeConfig = { ...this.#runtimeConfig, token };
  }

  setLogging(level: LogLevel): void {
    if (this.#closed) throw new Error("agent is closed");
    if (this.#runtimeConfig.logLevel === level) return;
    if (!["off", "error", "warn", "info", "debug", "trace"].includes(level))
      throw new TypeError("invalid logging level");
    this.#runtime?.set_log_level(level);
    this.#runtimeConfig = { ...this.#runtimeConfig, logLevel: level };
    this.#remote.setLogLevel(level);
  }

  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): Promise<void> {
    return this.#publishTopic(name, mode, payload);
  }

  close(): void {
    if (this.#closed) return;
    this.deactivate();
    this.#closed = true;
    const runtime = this.#runtime;
    this.#runtime = undefined;
    this.#runtimeGeneration = null;
    if (runtime) {
      runtime.set_snapshot_listener();
      runtime.set_event_listener();
      runtime.set_error_listener();
      runtime.close();
      runtime.abort();
    }
    this.#media.close();
    this.#resources.close();
    this.#data.close();
    this.#topics.close();
    this.#remote.close();
    this.#state = copyState({ connected: false });
    this.#localSourcePresent.clear();
    this.#localMuted.clear();
    for (const handle of this.#localVideoHandles.values()) handle.clear();
    for (const handle of this.#localAudioHandles.values()) handle.clear();
    this.#localVideoHandles.clear();
    this.#localAudioHandles.clear();
    this.#reservedSlots.clear();
    this.#localPublications.clear();
    this.#publish(emptySnapshot("disconnected"));
    this.#listeners.clear();
    this.#eventListeners.clear();
  }

  claimRelationship(owner: object): () => void {
    this.#relationships.add(owner);
    return () => {
      this.#relationships.delete(owner);
    };
  }

  claimPublication(
    owner: object,
    kind: "video" | "audio",
    label: string,
  ): LocalVideoTrack | LocalAudioTrack {
    const key = `${kind}:${label}`;
    if (this.#publicationOwners.has(key)) {
      const error = new RelationshipOwnershipError(
        `duplicate ${kind} publication owner for ${label}`,
      );
      this.#emitFailure("validation", error.message);
      throw error;
    }
    let handle: LocalVideoTrack | LocalAudioTrack;
    try {
      handle =
        kind === "video" ? this.local.video(label) : this.local.audio(label);
    } catch (error) {
      this.#emitFailure("validation", message(error));
      throw error;
    }
    const slot = [...this.#reservedSlots].find(
      ([slot, value]) =>
        value === label && slot.startsWith(kind === "video" ? "v" : "a"),
    )![0];
    this.#publicationOwners.set(key, { owner, handle, slot });
    this.#relationships.add(owner);
    return handle;
  }

  updatePublication(
    owner: object,
    kind: "video" | "audio",
    label: string,
    source: CapturedTrack | null,
    enabled: boolean,
    config: SenderConfig,
  ): void {
    const declaration = this.#publicationOwners.get(`${kind}:${label}`);
    if (declaration?.owner !== owner) return;
    const { handle, slot } = declaration;
    const previousConfig = this.#localConfigs.get(slot);
    this.#localConfigs.set(slot, config);
    this.#jsxMutation = true;
    try {
      handle.setEnabled(enabled);
      const changed = handle.source !== source;
      if (kind === "video")
        (handle as LocalVideoTrack).setSource(
          source as CapturedVideoTrack | null,
        );
      else
        (handle as LocalAudioTrack).setSource(
          source as CapturedAudioTrack | null,
        );
      if (
        !changed &&
        source &&
        JSON.stringify(previousConfig) !== JSON.stringify(config)
      ) {
        void this.replaceLocalTrack(
          slot,
          nativeCaptureTrack(source, kind),
          config,
        ).catch(() => {});
      }
    } finally {
      this.#jsxMutation = false;
    }
  }

  releasePublication(
    owner: object,
    kind: "video" | "audio",
    label: string,
  ): void {
    const key = `${kind}:${label}`;
    const declaration = this.#publicationOwners.get(key);
    if (declaration?.owner !== owner) return;
    this.#jsxMutation = true;
    try {
      declaration.handle.setSource(null);
    } finally {
      this.#jsxMutation = false;
    }
    this.#publicationOwners.delete(key);
    this.#relationships.delete(owner);
  }

  #assertSlotWritable(slot: string): void {
    if (
      !this.#jsxMutation &&
      [...this.#publicationOwners.values()].some((value) => value.slot === slot)
    ) {
      throw new RelationshipOwnershipError(
        `slot ${slot} is owned by committed JSX`,
      );
    }
  }

  #requireRuntime(): Runtime {
    if (this.#closed) throw new Error("agent is closed");
    if (!this.#runtime) throw new Error("agent is still initializing");
    return this.#runtime;
  }

  #effectiveLocalMuted(slot: string): boolean {
    return (
      this.#localSourcePresent.get(slot) === false ||
      (this.#localMuted.get(slot) ?? false)
    );
  }

  #runLocal(operation: (runtime: Runtime) => Promise<void>): Promise<void> {
    if (this.#closed) return Promise.reject(new Error("agent is closed"));
    const ready = this.#ready;
    if (!ready) return Promise.reject(new Error("agent is not active"));
    const activation = this.#activation;
    return ready
      .then(async (runtime) => {
        this.#requireCurrentRuntime(runtime);
        await operation(runtime);
        this.#requireCurrentRuntime(runtime);
      })
      .catch((error: unknown) => {
        if (this.#closed) throw new Error("agent is closed");
        if (
          activation === this.#activation &&
          this.#snapshot.failure === null
        ) {
          this.#emitFailure(localFailureClass(error), message(error));
        }
        throw error;
      });
  }

  #publishTopic(
    name: string,
    mode: TopicMode,
    payload: Uint8Array,
  ): Promise<void> {
    const ready = this.#ready;
    if (!ready || !this.#state.connected)
      return Promise.reject(new Error("topic publication is unavailable"));
    const activation = this.#activation;
    return new Promise((resolve, reject) => {
      let runtime: Runtime | undefined;
      let generation: bigint | null = null;
      let dataTrackId =
        this.#snapshot.topics.publishers.find(
          (status) => status.name === name && status.mode === mode,
        )?.dataTrackId ?? null;
      let dispatching = false;
      let settled = false;
      let unsubscribe = () => {};
      let unsubscribeEvents = () => {};
      let timer: ReturnType<typeof setTimeout> | undefined;
      const cleanup = () => {
        unsubscribe();
        unsubscribeEvents();
        if (timer !== undefined) clearTimeout(timer);
      };
      const fail = (error: unknown) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(error);
      };
      const check = () => {
        if (settled) return;
        if (
          this.#closed ||
          this.#activation !== activation ||
          !this.#state.connected ||
          (runtime !== undefined && runtime !== this.#runtime)
        ) {
          fail(
            new Error(
              "topic publication closed while awaiting its authoritative binding",
            ),
          );
          return;
        }
        if (generation !== null && this.#runtimeGeneration !== generation) {
          fail(new Error("topic publication generation was replaced"));
          return;
        }
        if (
          this.#snapshot.connection === "terminal-failure" ||
          this.#snapshot.connection === "closing"
        ) {
          fail(
            new Error(
              this.#snapshot.failure?.message ??
                "topic publication is unavailable",
            ),
          );
          return;
        }
        const publisher = this.#snapshot.topics.publishers.find(
          (status) => status.name === name && status.mode === mode,
        );
        if (publisher?.connected || publisher?.dataTrackId)
          generation ??= this.#runtimeGeneration;
        if (dataTrackId !== null && publisher?.dataTrackId !== dataTrackId) {
          fail(new Error("topic publication binding was replaced"));
          return;
        }
        dataTrackId ??= publisher?.dataTrackId ?? null;
        if (
          dispatching ||
          !runtime ||
          this.#snapshot.connection !== "connected" ||
          generation === null ||
          !publisher?.connected ||
          dataTrackId === null
        )
          return;
        dispatching = true;
        try {
          this.#requireCurrentRuntime(runtime);
          void runtime
            .send_topic(name, mode, payload, BigInt(generation), dataTrackId)
            .then(() => {
              if (settled) return;
              settled = true;
              cleanup();
              resolve();
            }, fail);
        } catch (error) {
          fail(error);
        }
      };
      unsubscribe = this.subscribe(check);
      unsubscribeEvents = this.subscribeEvents((event) => {
        if (
          event.type === "topic-channel-failed" &&
          event.direction === "publish" &&
          event.topic === name &&
          event.mode === mode
        )
          fail(new Error(event.message));
      });
      timer = setTimeout(
        () => fail(new Error("topic authoritative binding timed out")),
        10_000,
      );
      void ready.then((value) => {
        runtime = value;
        check();
      }, fail);
      check();
    });
  }

  #requireCurrentRuntime(runtime: Runtime): void {
    if (this.#closed || runtime !== this.#runtime) {
      throw new Error("agent is closed");
    }
  }

  #runtimeSnapshot(runtime: Runtime, raw: RuntimeSnapshot): void {
    if (this.#closed || runtime !== this.#runtime) return;
    this.#runtimeGeneration =
      raw.generation === undefined ? null : BigInt(raw.generation);
    const participants = Object.freeze(
      raw.participants.map((participant) => Object.freeze({ ...participant })),
    );
    const publications = Object.freeze(
      raw.publications.map((publication) => Object.freeze({ ...publication })),
    );
    const catalog = Object.freeze({
      revision: Number(raw.catalog.revision),
      participants,
      publications,
      tracks: Object.freeze(
        raw.catalog.tracks.map((track) => Object.freeze({ ...track })),
      ),
    });
    const mapping = Object.freeze({
      acceptedIntentRevision: Number(raw.mapping.acceptedIntentRevision),
      video: Object.freeze(
        raw.mapping.video.map((entry) => Object.freeze({ ...entry })),
      ),
      audio: Object.freeze(
        raw.mapping.audio.map((entry) => Object.freeze({ ...entry })),
      ),
    });
    const publicationById = new Map(
      publications.map((publication) => [publication.id, publication]),
    );
    const tracks: Record<string, RemoteTrack> = {};
    for (const binding of mapping.video) {
      const publication = publicationById.get(binding.publicationId);
      const media = runtime.remote_track(binding.receiverIndex);
      if (!publication || publication.kind !== "video" || !media) continue;
      tracks[publication.id] = Object.freeze({
        publicationId: publication.id,
        participantId: publication.participantId,
        kind: "video",
        media,
      });
    }
    for (const binding of mapping.audio) {
      const publication = publicationById.get(binding.publicationId);
      const media = runtime.remote_track(binding.receiverIndex);
      if (!publication || publication.kind !== "audio" || !media) continue;
      tracks[publication.id] = Object.freeze({
        publicationId: publication.id,
        participantId: publication.participantId,
        kind: "audio",
        media,
      });
    }
    const next: AgentSnapshot = Object.freeze({
      version: Number(raw.version),
      desiredRevision: Number(raw.desiredRevision),
      connection: raw.connection,
      generation:
        this.#runtimeGeneration === null
          ? null
          : Number(this.#runtimeGeneration),
      participantId: raw.participantId ?? null,
      roomId: raw.roomId ?? null,
      authorizationExpiresAt:
        raw.authorizationExpiresAt === undefined
          ? null
          : Number(raw.authorizationExpiresAt),
      catalog,
      mapping,
      tracks: Object.freeze(tracks),
      topics: freezeTopicSnapshot({
        ...raw.topics,
        publishers: raw.topics.publishers.map((publisher) => ({
          ...publisher,
          dataTrackId: publisher.dataTrackId ?? null,
        })),
        acceptedSends: Number(raw.topics.acceptedSends),
        droppedSends: Number(raw.topics.droppedSends),
        deliveredMessages: Number(raw.topics.deliveredMessages),
        channelFailures: Number(raw.topics.channelFailures),
      }),
      failure: raw.failure ? Object.freeze({ ...raw.failure }) : null,
    });
    this.#publish(next);
  }

  #runtimeEvent(raw: unknown): void {
    if (this.#closed || typeof raw !== "object" || raw === null) return;
    if (this.#data.notify(raw)) return;
    const value = raw as AgentEvent;
    const event = Object.freeze(
      value.type === "topic-message"
        ? { ...value, payload: value.payload.slice() }
        : { ...value },
    ) as AgentEvent;
    this.#emit(event);
  }

  #terminalFailure(failureClass: AgentError["class"], text: string): void {
    if (this.#closed) return;
    const failure = Object.freeze({ class: failureClass, message: text });
    this.#publish(
      Object.freeze({
        ...this.#snapshot,
        connection: "terminal-failure",
        failure,
      }),
    );
    this.#emit(Object.freeze({ type: "failure", ...failure }));
  }

  #emitFailure(failureClass: AgentError["class"], text: string): void {
    if (this.#closed) return;
    const failure = Object.freeze({ class: failureClass, message: text });
    if (this.#snapshot.connection !== "terminal-failure") {
      this.#publish(Object.freeze({ ...this.#snapshot, failure }));
    }
    this.#emit(Object.freeze({ type: "failure", ...failure }));
  }

  #publish(snapshot: AgentSnapshot): void {
    if (snapshot === this.#snapshot) return;
    this.#snapshot = snapshot;
    this.#remote.update(snapshot);
    for (const listener of [...this.#listeners]) listener();
  }

  #emit(event: AgentEvent): void {
    for (const listener of [...this.#eventListeners]) listener(event);
  }
}

export class RelationshipOwnershipError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RelationshipOwnershipError";
  }
}

export interface ManagedAgent extends Agent {
  activate(): void;
  deactivate(): void;
}

export function createManagedAgent(config: AgentConfig): ManagedAgent {
  return new AgentFacade(config, true);
}

export function createAgent(config: AgentConfig): Agent {
  const raw = new AgentFacade(config);
  const agent: Agent = Object.freeze({
    get connection() {
      return raw.connection;
    },
    get error() {
      return raw.error;
    },
    get participantId() {
      return raw.participantId;
    },
    get roomId() {
      return raw.roomId;
    },
    get authorizationExpiresAt() {
      return raw.authorizationExpiresAt;
    },
    get remote() {
      return raw.remote;
    },
    track: raw.track.bind(raw) as Agent["track"],
    connect: () => raw.connect(),
    disconnect: () => raw.disconnect(),
    renewAuthorization: (token: string) => raw.renewAuthorization(token),
    setLogging: (level: LogLevel) => raw.setLogging(level),
    getSnapshot: raw.getSnapshot,
    subscribe: raw.subscribe,
    close: () => raw.close(),
  });
  raw.bindPublicAgent(agent);
  return agent;
}
