import type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentFailure,
  AgentSnapshot,
  AgentState,
  AudioBinding,
  ConnectionState,
  LogLevel,
  MediaTopology,
  Participant,
  Publication,
  RemoteTrack,
  SenderConfig,
  TopicMode,
  TopicPublisherStatus,
  TopicSnapshot,
  TopicSubscriberStatus,
  VideoBinding,
} from "./types.js";
import { BrowserRuntime, whenInitialized } from "./wasm.js";

type Runtime = InstanceType<typeof BrowserRuntime>;

interface RuntimeSnapshot {
  readonly version: number;
  readonly desiredRevision: number;
  readonly connection: ConnectionState;
  readonly generation?: number;
  readonly participantId?: string;
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly video: readonly VideoBinding[];
  readonly audio: readonly (Omit<AudioBinding, "levelDbov"> & {
    readonly levelDbov?: number;
  })[];
  readonly topics: TopicSnapshot;
  readonly failure?: AgentFailure;
}

interface RuntimeConfig {
  readonly endpoint: string;
  readonly token: string;
  readonly topology: Required<MediaTopology>;
  readonly logLevel: LogLevel;
}

const EMPTY_ARRAY: readonly never[] = Object.freeze([]);
const EMPTY_TRACKS: Readonly<Record<string, RemoteTrack>> = Object.freeze({});

function emptyTopics(): TopicSnapshot {
  return Object.freeze({
    publishers: EMPTY_ARRAY,
    subscribers: EMPTY_ARRAY,
    acceptedSends: 0,
    droppedSends: 0,
    deliveredMessages: 0,
    resynchronizations: 0,
    channelFailures: 0,
  });
}

function emptySnapshot(connection: ConnectionState): AgentSnapshot {
  return Object.freeze({
    version: 0,
    desiredRevision: 0,
    connection,
    generation: null,
    participantId: null,
    participants: EMPTY_ARRAY,
    publications: EMPTY_ARRAY,
    video: EMPTY_ARRAY,
    audio: EMPTY_ARRAY,
    tracks: EMPTY_TRACKS,
    topics: emptyTopics(),
    failure: null,
  });
}

function copyConfig(config: AgentConfig): RuntimeConfig {
  return Object.freeze({
    endpoint: config.endpoint,
    token: config.token,
    topology: Object.freeze({
      localVideo: Object.freeze([...(config.topology.localVideo ?? [])]),
      localAudio: Object.freeze([...(config.topology.localAudio ?? [])]),
      remoteVideo: config.topology.remoteVideo ?? 0,
      remoteAudio: config.topology.remoteAudio ?? 0,
    }),
    logLevel: config.logging?.level ?? "warn",
  });
}

function copyState(state: AgentState): AgentState {
  return Object.freeze({
    connected: state.connected,
    publications: Object.freeze(
      (state.publications ?? []).map((publication) =>
        Object.freeze({ ...publication }),
      ),
    ),
    video: Object.freeze(
      (state.video ?? []).map((demand) => Object.freeze({ ...demand })),
    ),
    audio: Object.freeze({
      pinned: Object.freeze([...(state.audio?.pinned ?? [])]),
      automatic: state.audio?.automatic ?? true,
    }),
    playoutDelay: state.playoutDelay
      ? Object.freeze({ ...state.playoutDelay })
      : undefined,
    topics: Object.freeze(
      (state.topics ?? []).map((topic) => Object.freeze({ ...topic })),
    ),
  });
}

function desiredValue(state: AgentState): object {
  return {
    connected: state.connected,
    publications: state.publications,
    video: state.video,
    audio: state.audio,
    playoutDelay: state.playoutDelay ?? { mode: "adaptive" },
    topics: state.topics,
  };
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function localFailureClass(error: unknown): AgentFailure["class"] {
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

function freezeTopicSnapshot(topics: TopicSnapshot): TopicSnapshot {
  return Object.freeze({
    publishers: Object.freeze(
      topics.publishers.map((publisher) => Object.freeze({ ...publisher })),
    ) as readonly TopicPublisherStatus[],
    subscribers: Object.freeze(
      topics.subscribers.map((subscriber) => Object.freeze({ ...subscriber })),
    ) as readonly TopicSubscriberStatus[],
    acceptedSends: topics.acceptedSends,
    droppedSends: topics.droppedSends,
    deliveredMessages: topics.deliveredMessages,
    resynchronizations: topics.resynchronizations,
    channelFailures: topics.channelFailures,
  });
}

class AgentFacade implements Agent {
  #snapshot: AgentSnapshot = emptySnapshot("disconnected");
  #listeners = new Set<() => void>();
  #eventListeners = new Set<(event: AgentEvent) => void>();
  #state: AgentState = copyState({ connected: false });
  #runtime: Runtime | undefined;
  #localOperations = new Map<string, Promise<void>>();
  #localTracks = new Map<string, MediaStreamTrack>();
  #closed = false;
  readonly #ready: Promise<Runtime>;

  constructor(config: AgentConfig) {
    const runtimeConfig = copyConfig(config);
    this.#ready = whenInitialized()
      .catch((error: unknown) => {
        this.#terminalFailure("initialization", message(error));
        throw error;
      })
      .then(() => {
        if (this.#closed) throw new Error("agent is closed");
        let runtime: Runtime;
        try {
          runtime = new BrowserRuntime(runtimeConfig);
        } catch (error) {
          this.#terminalFailure("invalid-configuration", message(error));
          throw error;
        }
        this.#runtime = runtime;
        runtime.set_event_listener((event: unknown) =>
          this.#runtimeEvent(event),
        );
        runtime.set_error_listener((error: unknown) =>
          this.#emitFailure(localFailureClass(error), message(error)),
        );
        try {
          runtime.replace_desired(desiredValue(this.#state));
        } catch (error) {
          this.#terminalFailure("invalid-configuration", message(error));
          runtime.abort();
          this.#runtime = undefined;
          throw error;
        }
        runtime.set_snapshot_listener((snapshot: RuntimeSnapshot) =>
          this.#runtimeSnapshot(runtime, snapshot),
        );
        return runtime;
      });
    void this.#ready.catch(() => {});
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

  setState(state: AgentState): void {
    if (this.#closed) return;
    this.#state = copyState(state);
    if (!this.#runtime) {
      if (this.#snapshot.connection === "terminal-failure") return;
      this.#publish(
        Object.freeze({
          ...this.#snapshot,
          connection: this.#state.connected ? "initializing" : "disconnected",
          failure: null,
        }),
      );
    } else {
      try {
        this.#runtime.replace_desired(desiredValue(this.#state));
      } catch (error) {
        this.#emitFailure(localFailureClass(error), message(error));
      }
    }
  }

  replaceLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    config: SenderConfig,
  ): Promise<void> {
    const sender = Object.freeze({
      contentHint: config.contentHint,
      degradationPreference: config.degradationPreference,
      encodings: Object.freeze(
        (config.encodings ?? []).map((encoding) =>
          Object.freeze({ ...encoding }),
        ),
      ),
    });
    return this.#queueLocal(slot, async (runtime) => {
      await runtime.replace_local_track(slot, track, sender);
      this.#requireCurrentRuntime(runtime);
      if (track) {
        this.#localTracks.set(slot, track);
      } else {
        this.#localTracks.delete(slot);
      }
    });
  }

  setLocalMuted(slot: string, muted: boolean): Promise<void> {
    return this.#queueLocal(slot, async (runtime) => {
      await runtime.set_local_muted(slot, muted);
      const track = this.#localTracks.get(slot);
      if (track) track.enabled = !muted;
    });
  }

  reconnect(): void {
    this.#requireRuntime().force_reconnect();
  }

  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): void {
    this.#requireRuntime().send_topic(name, mode, payload);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    const runtime = this.#runtime;
    this.#runtime = undefined;
    if (runtime) {
      runtime.set_snapshot_listener();
      runtime.set_event_listener();
      runtime.set_error_listener();
      runtime.close();
      runtime.abort();
    }
    this.#state = copyState({ connected: false });
    this.#localOperations.clear();
    this.#localTracks.clear();
    this.#publish(emptySnapshot("disconnected"));
    this.#listeners.clear();
    this.#eventListeners.clear();
  }

  #requireRuntime(): Runtime {
    if (this.#closed) throw new Error("agent is closed");
    if (!this.#runtime) throw new Error("agent is still initializing");
    return this.#runtime;
  }

  #queueLocal(
    slot: string,
    operation: (runtime: Runtime) => Promise<void>,
  ): Promise<void> {
    if (this.#closed) return Promise.reject(new Error("agent is closed"));
    const prior = this.#localOperations.get(slot) ?? Promise.resolve();
    const current = prior
      .catch(() => {})
      .then(async () => {
        const runtime = await this.#ready;
        if (this.#closed || runtime !== this.#runtime) {
          throw new Error("agent is closed");
        }
        await operation(runtime);
        this.#requireCurrentRuntime(runtime);
      })
      .catch((error: unknown) => {
        if (this.#closed) {
          throw new Error("agent is closed");
        }
        if (!this.#closed && this.#snapshot.failure === null) {
          this.#emitFailure(localFailureClass(error), message(error));
        }
        throw error;
      });
    this.#localOperations.set(slot, current);
    void current
      .finally(() => {
        if (this.#localOperations.get(slot) === current) {
          this.#localOperations.delete(slot);
        }
      })
      .catch(() => {});
    return current;
  }

  #requireCurrentRuntime(runtime: Runtime): void {
    if (this.#closed || runtime !== this.#runtime) {
      throw new Error("agent is closed");
    }
  }

  #runtimeSnapshot(runtime: Runtime, raw: RuntimeSnapshot): void {
    if (this.#closed || runtime !== this.#runtime) return;
    const participants = Object.freeze(
      raw.participants.map((participant) => Object.freeze({ ...participant })),
    );
    const publications = Object.freeze(
      raw.publications.map((publication) => Object.freeze({ ...publication })),
    );
    const video = Object.freeze(
      raw.video.map((binding) => Object.freeze({ ...binding })),
    );
    const audio = Object.freeze(
      raw.audio.map((binding) =>
        Object.freeze({ ...binding, levelDbov: binding.levelDbov ?? null }),
      ),
    );
    const publicationById = new Map(
      publications.map((publication) => [publication.id, publication]),
    );
    const tracks: Record<string, RemoteTrack> = {};
    for (const binding of video) {
      const publication = publicationById.get(binding.trackId);
      const media = runtime.remote_track(binding.mid);
      if (!publication || publication.kind !== "video" || !media) continue;
      tracks[publication.id] = Object.freeze({
        publicationId: publication.id,
        participantId: publication.participantId,
        kind: "video",
        mid: binding.mid,
        paused: binding.paused,
        media,
      });
    }
    for (const binding of audio) {
      const publication = publicationById.get(binding.trackId);
      const media = runtime.remote_track(binding.mid);
      if (!publication || publication.kind !== "audio" || !media) continue;
      tracks[publication.id] = Object.freeze({
        publicationId: publication.id,
        participantId: publication.participantId,
        kind: "audio",
        mid: binding.mid,
        levelDbov: binding.levelDbov,
        media,
      });
    }
    this.#publish(
      Object.freeze({
        version: raw.version,
        desiredRevision: raw.desiredRevision,
        connection: raw.connection,
        generation: raw.generation ?? null,
        participantId: raw.participantId ?? null,
        participants,
        publications,
        video,
        audio,
        tracks: Object.freeze(tracks),
        topics: freezeTopicSnapshot(raw.topics),
        failure: raw.failure ? Object.freeze({ ...raw.failure }) : null,
      }),
    );
  }

  #runtimeEvent(raw: unknown): void {
    if (this.#closed || typeof raw !== "object" || raw === null) return;
    const value = raw as AgentEvent;
    const event = Object.freeze(
      value.type === "topic-message"
        ? { ...value, payload: value.payload.slice() }
        : { ...value },
    ) as AgentEvent;
    this.#emit(event);
  }

  #terminalFailure(failureClass: AgentFailure["class"], text: string): void {
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

  #emitFailure(failureClass: AgentFailure["class"], text: string): void {
    if (this.#closed) return;
    this.#emit(
      Object.freeze({ type: "failure", class: failureClass, message: text }),
    );
  }

  #publish(snapshot: AgentSnapshot): void {
    if (snapshot === this.#snapshot) return;
    this.#snapshot = snapshot;
    for (const listener of [...this.#listeners]) listener();
  }

  #emit(event: AgentEvent): void {
    for (const listener of [...this.#eventListeners]) listener(event);
  }
}

export function createAgent(config: AgentConfig): Agent {
  return new AgentFacade(config);
}
