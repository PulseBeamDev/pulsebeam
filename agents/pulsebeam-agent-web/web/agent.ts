import { DataRegistry, type LocalDataTrack } from "./data.js";
import { track, type Delivery } from "./track.js";
import { selectAudio } from "./reception.js";
import { observeSnapshots, observeEvents } from "./observation.js";
import type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentError,
  AgentSnapshot,
  AgentState,
  AudioDemand,
  CatalogSnapshot,
  ConnectionState,
  LogLevel,
  LocalAudioTrack,
  LocalVideoTrack,
  MappingSnapshot,
  Participant,
  Publication,
  RemoteTrack,
  RemoteParticipant,
  Topic,
  TopicMode,
  TopicRegistration,
  TopicSnapshot,
  TopicCallbacks,
  TopicPublisherStatus,
} from "./types.js";
import { RemoteCatalog } from "./remote-catalog.js";
import { TopicRegistry } from "./topic.js";
import { ResourceError, ResourceOwners } from "./resources.js";
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
  readonly #snapshots = new Set<ReturnType<typeof observeSnapshots>>();
  readonly #events = new Set<ReturnType<typeof observeEvents>>();
  #state: AgentState = copyState({ connected: false });
  #runtime: Runtime | undefined;
  #runtimeGeneration: bigint | null = null;
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
  #closed = false;
  #closing: Promise<void> | undefined;
  #active = false;
  #activation = 0;
  #ready: Promise<Runtime> | undefined;
  #runtimeConfig: RuntimeConfig;

  constructor(config: AgentConfig, managed = false) {
    const runtimeConfig = copyConfig(config);
    this.#runtimeConfig = runtimeConfig;
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
        return runtime;
      });
    void this.#ready.catch(() => {});
  }

  deactivate(): void {
    if (!this.#active) return;
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

  async initialized(): Promise<void> {
    await this.#ready;
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
  }

  bindPublicAgent(agent: Agent): void {
    this.#remote.bindAgent(agent);
  }

  video(label: string): LocalVideoTrack {
    return this.#media.track(track.video(label));
  }
  audio(label: string): LocalAudioTrack {
    return this.#media.track(track.audio(label));
  }
  data<D extends Delivery>(label: string, delivery: D): LocalDataTrack<D> {
    return this.#data.local(track.data(label, delivery));
  }
  get participants(): readonly RemoteParticipant[] {
    return this.#remote.facade().participants;
  }
  participant(id: string): RemoteParticipant {
    return this.#remote.facade().participant(id);
  }
  selectAudio(policy: AudioDemand) {
    return selectAudio(this.#resources, policy);
  }
  isClosed(): boolean {
    return this.#closed;
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

  snapshot(): AgentSnapshot {
    return this.#snapshot;
  }
  snapshots() {
    const observation = observeSnapshots(this.#snapshot, this.#closed, () =>
      this.#snapshots.delete(observation),
    );
    if (!this.#closed) this.#snapshots.add(observation);
    return observation.stream;
  }
  events() {
    const observation = observeEvents(this.#closed, () =>
      this.#events.delete(observation),
    );
    if (!this.#closed) this.#events.add(observation);
    return observation.stream;
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

  async connect(): Promise<void> {
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
    if (this.#snapshot.connection === "terminal-failure" && !this.#runtime) {
      this.deactivate();
      this.activate();
    }
    if (!this.#state.connected)
      this.#applyState({ ...this.#state, connected: true });
    else this.#runtime?.connect();
    await this.#ready;
  }

  async disconnect(): Promise<void> {
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
    if (this.#state.connected)
      this.#applyState({ ...this.#state, connected: false });
    await this.#ready;
  }

  setState(state: AgentState): void {
    if (this.#closed) return;
    validatePlayoutDelay(state);
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
      publications: state.publications ?? [],
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

  reconnect(): void {
    this.#requireRuntime().force_reconnect();
  }

  async renewAuthorization(token: string): Promise<void> {
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
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

  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    const initialized = this.#ready;
    let resolve!: () => void;
    let reject!: (error: unknown) => void;
    this.#closing = new Promise<void>((done, failed) => {
      resolve = done;
      reject = failed;
    });
    this.#closed = true;
    for (const observation of this.#snapshots) observation.close();
    for (const observation of this.#events) observation.close();
    this.deactivate();
    this.#media.close();
    const released = [this.#resources.close(), this.#data.close()];
    this.#topics.close();
    this.#remote.close();
    this.#state = copyState({ connected: false });
    this.#publish(emptySnapshot("disconnected"));
    this.#listeners.clear();
    this.#eventListeners.clear();
    void Promise.all([...released, initialized?.catch(() => {})]).then(
      () => resolve(),
      reject,
    );
    return this.#closing;
  }

  #requireRuntime(): Runtime {
    if (this.#closed) throw new Error("agent is closed");
    if (!this.#runtime) throw new Error("agent is still initializing");
    return this.#runtime;
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
    for (const observation of this.#snapshots) observation.publish(snapshot);
    this.#remote.update(snapshot);
    for (const listener of [...this.#listeners]) listener();
  }

  #emit(event: AgentEvent): void {
    for (const observation of this.#events) observation.publish(event);
    for (const listener of [...this.#eventListeners]) listener(event);
  }
}

export interface ManagedAgent extends Agent {
  readonly participants: readonly RemoteParticipant[];
  readonly getSnapshot: () => AgentSnapshot;
  readonly subscribe: (listener: () => void) => () => void;
  activate(): void;
  deactivate(): void;
}

export function createManagedAgent(config: AgentConfig): ManagedAgent {
  return new AgentFacade(config, true);
}

export async function createAgent(config: AgentConfig): Promise<Agent> {
  const raw = new AgentFacade(config);
  try {
    await raw.initialized();
  } catch (error) {
    await raw.close();
    throw error;
  }
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
    video: (label: string) => raw.video(label),
    audio: (label: string) => raw.audio(label),
    data: <D extends Delivery>(label: string, delivery: D) =>
      raw.data(label, delivery),
    participant: (id: string) => raw.participant(id),
    selectAudio: (policy: AudioDemand) => raw.selectAudio(policy),
    isClosed: () => raw.isClosed(),
    connect: () => raw.connect(),
    disconnect: () => raw.disconnect(),
    renewAuthorization: (token: string) => raw.renewAuthorization(token),
    setLogging: (level: LogLevel) => raw.setLogging(level),
    snapshot: () => raw.snapshot(),
    snapshots: () => raw.snapshots(),
    events: () => raw.events(),
    close: () => raw.close(),
  });
  raw.bindPublicAgent(agent);
  return agent;
}
