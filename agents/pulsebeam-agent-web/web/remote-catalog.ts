import { ErrorCode } from "./schema.js";
import type { ReconciliationSnapshot } from "./reconciliation-snapshot.js";
import { OperationError, raise } from "./agent-observation.js";
import {
  ResourceObservation,
  type NativeResourceObserver,
} from "./resource-observation.js";
import type { MediaHost, MediaAttachment } from "./remote-media.js";
import { attachMedia } from "./media-attachment.js";
import {
  claimVideoSubscription,
  defaultVideoObservation,
  defaultAudioSubscription,
  subscribeAudio,
  subscribeVideo,
} from "./reception.js";
import { RemoteAudioPlayback } from "./remote-audio.js";
import {
  ResourceError,
  ResourceOwners,
  type ResourceBinding,
} from "./resources.js";
import type { DataRegistry, RemoteDataTrack } from "./data.js";
import { track, type Delivery } from "./track.js";
import type {
  Agent,
  AudioSubscriptionConfig,
  VideoObservation,
  LogLevel,
  PlaybackFailure,
  ReceiveOptions,
  RemoteAudioTrack,
  RemoteParticipant,
  RemoteMediaAttachmentOptions,
  RemoteVideoTrack,
  RoomRef,
  Watch,
  WatchConfig,
  WatchState,
} from "./types.js";

type PlaybackCallback = (
  failure: PlaybackFailure,
  retry: () => Promise<void>,
) => void;
export interface RemoteVideoAttachment extends MediaAttachment {
  readonly ready: Promise<void>;
  setPreferences(options: ReceiveOptions): void;
}
interface NativeWatch extends NativeResourceObserver {
  close(): void;
}

export function watchCatalog(
  runtime: {
    catalog_watch(room: string, limit: number, cursor: Uint8Array): NativeWatch;
  },
  room: string,
  config: WatchConfig = {},
): Watch {
  const invalid = () =>
    new OperationError({
      code: ErrorCode.InvalidArgument,
      message: "Invalid watch configuration",
    });
  if (!config || typeof config !== "object") throw invalid();
  const limit = config.limit ?? 100;
  if (!Number.isInteger(limit) || limit < 0 || limit > 0xffff_ffff)
    throw invalid();
  const bytes = config.cursor ?? [];
  if (!(bytes instanceof Uint8Array) && !Array.isArray(bytes)) throw invalid();
  if (
    !Array.from(bytes).every(
      (byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255,
    )
  )
    throw invalid();
  const cursor = Uint8Array.from(bytes);
  try {
    const native = runtime.catalog_watch(room, limit, cursor);
    // BrowserRuntime freezes the complete projection, including the shared page.
    const observation = new ResourceObservation<WatchState>((value) => value);
    observation.bind(native);
    return Object.freeze({
      state: () => observation.state(),
      updates: () => observation.updates(),
      close: () => {
        native.close();
        return Promise.resolve();
      },
    });
  } catch (error) {
    return raise(error);
  }
}

const MAX_UINT32 = 0xffff_ffff;
const videoOwners = new WeakMap<RemoteVideoTrack, VideoHandle>();
const audioCatalogs = new WeakMap<object, RemoteCatalog>();

export interface RemoteAudioAttachment {
  readonly ready: Promise<void>;
  close(): Promise<void>;
}
export interface RemoteAudioAttachmentOptions {
  readonly onPlaybackError?: (failure: PlaybackFailure) => void;
}

export function attachRemoteAudio(
  agent: Agent,
  options: RemoteAudioAttachmentOptions = {},
): RemoteAudioAttachment {
  const catalog = audioCatalogs.get(agent);
  if (!catalog) throw new TypeError("invalid agent");
  return catalog.attachAudio(options);
}

function uint32(value: number, field: string): number {
  if (!Number.isInteger(value) || value < 0 || value > MAX_UINT32)
    throw new TypeError(`${field} must be a finite uint32 integer`);
  return value;
}

export function validateReceiveOptions(
  options: ReceiveOptions,
): ReceiveOptions {
  if (
    options.playoutDelay &&
    options.playoutDelay.minMs > options.playoutDelay.maxMs
  )
    throw new RangeError("latency minimum must not exceed maximum");
  return receiveOptions(options);
}

function receiveOptions(options: ReceiveOptions): ReceiveOptions {
  return Object.freeze({
    minHeight: uint32(options.minHeight ?? 0, "minHeight"),
    minFps: uint32(options.minFps ?? 0, "minFps"),
    priority: uint32(options.priority ?? 0, "priority"),
    ...(options.playoutDelay
      ? {
          playoutDelay: Object.freeze({
            minMs: uint32(options.playoutDelay.minMs, "playoutDelay.minMs"),
            maxMs: uint32(options.playoutDelay.maxMs, "playoutDelay.maxMs"),
          }),
        }
      : {}),
  });
}

class VideoHandle implements RemoteVideoTrack {
  readonly kind = "video" as const;
  constructor(
    readonly catalog: RemoteCatalog,
    readonly room: string,
    readonly participantId: string,
    readonly trackId: string,
  ) {
    videoOwners.set(this, this);
  }
  get id(): string | null {
    return this.catalog.resolve(
      this.room,
      this.participantId,
      "video",
      this.trackId,
    );
  }
  get active(): boolean {
    return this.id !== null;
  }
  watch(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    let id = this.id;
    return this.catalog.agent.subscribe(() => {
      const next = this.id;
      if (next === id) return;
      id = next;
      listener();
    });
  }
  subscribe(configuration: VideoObservation = defaultVideoObservation) {
    return subscribeVideo(
      this.catalog.owners,
      {
        room: this.room,
        participantId: this.participantId,
        trackId: this.trackId,
      },
      configuration,
    );
  }
}

class AudioHandle implements RemoteAudioTrack {
  readonly kind = "audio" as const;
  constructor(
    readonly catalog: RemoteCatalog,
    readonly room: string,
    readonly participantId: string,
    readonly trackId: string,
  ) {}
  get id(): string | null {
    return this.catalog.resolve(
      this.room,
      this.participantId,
      "audio",
      this.trackId,
    );
  }
  get active(): boolean {
    return this.id !== null;
  }
  get receiving(): boolean {
    const id = this.id;
    return (
      id !== null &&
      this.catalog.agent
        .getSnapshot()
        .mapping.audio.some((entry) => entry.publicationId === id)
    );
  }
  watch(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    let state = `${this.id}:${this.receiving}`;
    return this.catalog.agent.subscribe(() => {
      const next = `${this.id}:${this.receiving}`;
      if (next === state) return;
      state = next;
      listener();
    });
  }
  subscribe(configuration: AudioSubscriptionConfig = defaultAudioSubscription) {
    return subscribeAudio(
      this.catalog.owners,
      {
        room: this.room,
        participantId: this.participantId,
        trackId: this.trackId,
      },
      configuration,
    );
  }
}

class ParticipantHandle implements RemoteParticipant {
  readonly videos = new Map<string, VideoHandle>();
  readonly audios = new Map<string, AudioHandle>();
  get active(): boolean {
    return (
      !this.catalog.closed &&
      this.catalog.isCurrentRoom(this.room) &&
      this.catalog.agent
        .getSnapshot()
        .catalog.participants.some(
          (participant) => participant.participantId === this.participantId,
        )
    );
  }
  id(): string {
    return this.participantId;
  }
  data<D extends Delivery>(trackId: string, delivery: D): RemoteDataTrack<D> {
    return this.catalog.data.remote(track.data(trackId, delivery), {
      room: this.room,
      participantId: this.participantId,
    });
  }
  constructor(
    readonly catalog: RemoteCatalog,
    readonly room: string,
    readonly participantId: string,
  ) {}
  video(trackId: string): VideoHandle {
    this.catalog.requireOpen();
    let handle = this.videos.get(trackId);
    if (!handle) {
      handle = new VideoHandle(
        this.catalog,
        this.room,
        this.participantId,
        trackId,
      );
      this.videos.set(trackId, handle);
    }
    return handle;
  }
  audio(trackId: string): AudioHandle {
    this.catalog.requireOpen();
    let handle = this.audios.get(trackId);
    if (!handle) {
      handle = new AudioHandle(
        this.catalog,
        this.room,
        this.participantId,
        trackId,
      );
      this.audios.set(trackId, handle);
    }
    return handle;
  }
}

export class RemoteCatalog {
  readonly #handles = new Map<string, ParticipantHandle>();
  readonly #publicParticipants = new WeakMap<
    ParticipantHandle,
    RemoteParticipant
  >();
  readonly #publicVideos = new WeakMap<VideoHandle, RemoteVideoTrack>();
  readonly #publicAudios = new WeakMap<AudioHandle, RemoteAudioTrack>();
  #playback: RemoteAudioPlayback | undefined;
  #closed = false;
  constructor(
    readonly agent: MediaHost,
    readonly data: DataRegistry,
    readonly owners: ResourceOwners,
    private logLevel: LogLevel = "warn",
    private readonly openWatch?: (room: string, config?: WatchConfig) => Watch,
  ) {
    audioCatalogs.set(agent, this);
  }
  #wrapParticipant(participant: ParticipantHandle): RemoteParticipant {
    const wrapVideo = (handle: VideoHandle): RemoteVideoTrack => {
      let value = this.#publicVideos.get(handle);
      if (!value) {
        value = Object.freeze({
          kind: handle.kind,
          trackId: handle.trackId,
          participantId: handle.participantId,
          get id() {
            return handle.id;
          },
          get active() {
            return handle.active;
          },
          watch: (listener: () => void) => handle.watch(listener),
          subscribe: (configuration?: VideoObservation) =>
            handle.subscribe(configuration),
        });
        this.#publicVideos.set(handle, value);
        videoOwners.set(value, handle);
      }
      return value;
    };
    const wrapAudio = (handle: AudioHandle): RemoteAudioTrack => {
      let value = this.#publicAudios.get(handle);
      if (!value) {
        value = Object.freeze({
          kind: handle.kind,
          trackId: handle.trackId,
          participantId: handle.participantId,
          get id() {
            return handle.id;
          },
          get active() {
            return handle.active;
          },
          watch: (listener: () => void) => handle.watch(listener),
          subscribe: (configuration?: AudioSubscriptionConfig) =>
            handle.subscribe(configuration),
        });
        this.#publicAudios.set(handle, value);
      }
      return value;
    };
    const wrapParticipant = (handle: ParticipantHandle): RemoteParticipant => {
      let value = this.#publicParticipants.get(handle);
      if (value) return value;
      value = Object.freeze({
        room: handle.room,
        participantId: handle.participantId,
        id: () => handle.id(),
        get active() {
          return handle.active;
        },
        video: (trackId: string) => wrapVideo(handle.video(trackId)),
        audio: (trackId: string) => wrapAudio(handle.audio(trackId)),
        data: <D extends Delivery>(trackId: string, delivery: D) =>
          handle.data(trackId, delivery),
      });
      this.#publicParticipants.set(handle, value);
      return value;
    };
    return wrapParticipant(participant);
  }
  bindAgent(agent: Agent): void {
    audioCatalogs.set(agent, this);
  }
  setLogLevel(level: LogLevel): void {
    this.logLevel = level;
    this.#playback?.setLogLevel(level);
  }

  attachAudio(options: RemoteAudioAttachmentOptions): RemoteAudioAttachment {
    this.requireOpen();
    let playback: RemoteAudioPlayback | undefined;
    const retry = async () => {
      const binding = owner.binding;
      const current = playback;
      if (!binding || !owner.current(binding) || !current) return;
      current.update(this.agent.getSnapshot());
      try {
        await current.resume();
      } catch (error) {
        if (owner.current(binding)) throw error;
      }
    };
    const report = (error: unknown) => {
      const binding = owner.binding;
      if (!binding || !owner.current(binding)) return;
      const failure: PlaybackFailure = Object.freeze({
        message: error instanceof Error ? error.message : String(error),
        cause: error,
        retry,
      });
      if (options.onPlaybackError) {
        try {
          options.onPlaybackError(failure);
        } catch (error) {
          console.error("PulseBeam playback error callback failed", error);
        }
      } else if (this.logLevel !== "off" && this.logLevel !== "error") {
        console.warn(
          "PulseBeam audio playback unavailable; retry from a user gesture",
          failure,
        );
      }
    };
    const owner = this.owners.acquire(
      { op: "claimAudio" },
      {
        bind: (binding) => {
          void Promise.resolve()
            .then(() => {
              if (!owner.current(binding)) return;
              playback = new RemoteAudioPlayback(this.logLevel, report);
              this.#playback = playback;
              playback.start(this.agent.getSnapshot());
            })
            .catch((error: unknown) => owner.fail(error));
        },
        unbind: () => {
          playback?.close();
          if (playback && this.#playback === playback)
            this.#playback = undefined;
          playback = undefined;
        },
      },
    );
    return Object.freeze({ ready: owner.ready, close: () => owner.close() });
  }

  get closed(): boolean {
    return this.#closed;
  }
  requireOpen(): void {
    if (this.#closed) throw new Error("agent is closed");
  }
  isCurrentRoom(room: string): boolean {
    return this.agent.getSnapshot().roomId === room;
  }
  resolve(
    room: string,
    participantId: string,
    kind: "audio" | "video",
    trackId: string,
  ): string | null {
    if (this.#closed || !this.isCurrentRoom(room)) return null;
    return (
      this.agent
        .getSnapshot()
        .catalog.publications.find(
          (publication) =>
            publication.participantId === participantId &&
            publication.kind === kind &&
            publication.trackId === trackId,
        )?.id ?? null
    );
  }
  room(name: string): RoomRef {
    this.requireOpen();
    return Object.freeze({
      name,
      watch: (config?: WatchConfig) => {
        if (this.#closed)
          throw new OperationError({
            code: ErrorCode.Closed,
            message: "Agent is closed",
          });
        if (!this.openWatch)
          throw new OperationError({
            code: ErrorCode.NotReady,
            message: "Watch runtime is inactive",
          });
        return this.openWatch(name, config);
      },
      participant: (id: string) => {
        this.requireOpen();
        return this.#wrapParticipant(this.#discovered(name, id));
      },
    });
  }
  #discovered(room: string, participantId: string): ParticipantHandle {
    const key = JSON.stringify([room, participantId]);
    let handle = this.#handles.get(key);
    if (!handle) {
      handle = new ParticipantHandle(this, room, participantId);
      this.#handles.set(key, handle);
    }
    return handle;
  }
  resumeAudio(): Promise<void> {
    return (
      this.#playback?.resume() ??
      Promise.reject(
        new ResourceError(
          "unavailable",
          "audio playback has no committed owner",
        ),
      )
    );
  }
  resetPlayback(): void {
    this.#playback?.reset();
  }
  update(snapshot: ReconciliationSnapshot): void {
    if (this.#closed) return;
    this.#playback?.update(snapshot);
  }
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.owners.close();
    this.#playback?.close();
    this.#playback = undefined;
    audioCatalogs.delete(this.agent);
    this.#handles.clear();
  }
}

export function attachRemoteVideo(
  source: RemoteVideoTrack,
  element: HTMLVideoElement,
  onPlaybackBlocked?: PlaybackCallback,
  options: Pick<RemoteMediaAttachmentOptions, "autoPlay"> & {
    readonly preferences?: ReceiveOptions;
  } = {},
): RemoteVideoAttachment {
  const handle = videoOwners.get(source);
  if (!handle) throw new TypeError("invalid remote video source");
  if (handle.catalog.closed)
    throw new ResourceError("closed", "agent is closed");
  return attachVideoHandle(handle, element, onPlaybackBlocked, options);
}

function attachVideoHandle(
  handle: VideoHandle,
  element: HTMLVideoElement,
  onPlaybackBlocked: PlaybackCallback | undefined,
  options: Pick<RemoteMediaAttachmentOptions, "autoPlay"> & {
    readonly preferences?: ReceiveOptions;
  },
): RemoteVideoAttachment {
  if (!(element instanceof HTMLVideoElement))
    throw new TypeError("remote attachment requires a video element");
  const configuration = (options: ReceiveOptions): VideoObservation => {
    const preferences = validateReceiveOptions(options);
    return {
      visible: true,
      height: 0,
      minHeight: preferences.minHeight ?? 0,
      minFps: preferences.minFps ?? 0,
      priority: preferences.priority ?? 0,
      playoutDelay: preferences.playoutDelay
        ? { mode: "fixed", ...preferences.playoutDelay }
        : { mode: "adaptive" },
    };
  };
  const claim = claimVideoSubscription(
    handle.catalog.owners,
    {
      room: handle.room,
      participantId: handle.participantId,
      trackId: handle.trackId,
    },
    configuration(options.preferences ?? {}),
  );
  const report = (error: unknown, retry: () => Promise<void>) =>
    onPlaybackBlocked?.(
      {
        message: error instanceof Error ? error.message : String(error),
        cause: error,
        retry,
      },
      retry,
    );
  const attachment = attachMedia(
    claim.subscription,
    "video",
    element,
    options.autoPlay ?? true,
    report,
    (error) => report(error, () => claim.ready),
    claim.ready,
  );
  let closing: Promise<void> | undefined;
  return {
    ready: claim.ready,
    setAutoPlay: attachment.setAutoPlay,
    setPreferences: (next) => {
      if (closing) return;
      const value = configuration(next);
      void claim.subscription
        .configure(value)
        .catch((error: unknown) =>
          report(error, () => claim.subscription.configure(value)),
        );
    },
    retryPlayback: attachment.retryPlayback,
    close: () =>
      (closing ??= Promise.allSettled([
        attachment.close(),
        claim.subscription.close(),
      ]).then(() => {})),
  };
}
