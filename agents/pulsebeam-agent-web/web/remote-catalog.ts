import {
  attachRemoteMedia,
  type MediaHost,
  type MediaAttachment,
} from "./remote-media.js";
import {
  claimVideoSubscription,
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
  AgentSnapshot,
  AudioSubscriptionConfig,
  VideoObservation,
  LogLevel,
  PlaybackFailure,
  ReceiveOptions,
  RemoteAudioTrack,
  RemoteParticipant,
  RemotePublishedTrack,
  RemoteMediaAttachmentOptions,
  RemoteVideoTrack,
  RemoteMedia,
} from "./types.js";

type PlaybackCallback = (
  failure: PlaybackFailure,
  retry: () => Promise<void>,
) => void;
export interface RemoteVideoAttachment extends MediaAttachment {
  readonly ready: Promise<void>;
  setPreferences(options: ReceiveOptions): void;
}
const MAX_UINT32 = 0xffff_ffff;
const videoOwners = new WeakMap<RemoteVideoTrack, VideoHandle>();
const audioCatalogs = new WeakMap<Agent, RemoteCatalog>();

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

function collection<T>(prior: readonly T[], next: T[]): readonly T[] {
  return prior.length === next.length &&
    prior.every((item) => next.includes(item))
    ? prior
    : Object.freeze(next);
}

class VideoHandle implements RemoteVideoTrack {
  readonly kind = "video" as const;
  publicationId: string | undefined;
  readonly bindingListeners = new Set<() => void>();
  readonly optionListeners = new Set<() => void>();
  readonly #listeners = new Set<() => void>();
  options: ReceiveOptions = validateReceiveOptions({});
  constructor(
    readonly catalog: RemoteCatalog,
    readonly participantId: string,
    readonly label: string,
  ) {
    videoOwners.set(this, this);
  }
  get id(): string | null {
    return this.publicationId ?? null;
  }
  get active(): boolean {
    return this.publicationId !== undefined;
  }
  watch(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }
  subscribe(configuration: VideoObservation) {
    return subscribeVideo(
      this.catalog.owners,
      {
        participantId: this.participantId,
        label: this.label,
      },
      configuration,
    );
  }
  bind(id: string | undefined): void {
    if (id === this.publicationId) return;
    this.publicationId = id;
    for (const listener of this.bindingListeners) listener();
    for (const listener of this.#listeners) listener();
  }
  setReceiveOptions(options: ReceiveOptions): void {
    if (this.catalog.closed) return;
    const next = validateReceiveOptions(options);
    if (JSON.stringify(this.options) === JSON.stringify(next)) return;
    this.options = next;
    for (const listener of this.optionListeners) listener();
  }
  close(): void {
    this.bind(undefined);
    this.bindingListeners.clear();
    this.optionListeners.clear();
    this.#listeners.clear();
  }
}

class AudioHandle implements RemoteAudioTrack {
  readonly kind = "audio" as const;
  publicationId: string | undefined;
  #binding: number | undefined;
  readonly #listeners = new Set<() => void>();
  constructor(
    readonly catalog: RemoteCatalog,
    readonly participantId: string,
    readonly label: string,
  ) {}
  get id(): string | null {
    return this.publicationId ?? null;
  }
  get active(): boolean {
    return this.publicationId !== undefined;
  }
  get receiving(): boolean {
    return this.#binding !== undefined;
  }
  bind(id: string | undefined, receiver: number | undefined): void {
    const changed = id !== this.publicationId || receiver !== this.#binding;
    this.publicationId = id;
    this.#binding = receiver;
    if (changed) for (const listener of this.#listeners) listener();
  }
  watch(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }
  subscribe(configuration: AudioSubscriptionConfig) {
    return subscribeAudio(
      this.catalog.owners,
      {
        participantId: this.participantId,
        label: this.label,
      },
      configuration,
    );
  }
  readWaveform(buffer: Float32Array): void {
    this.catalog.read(this.publicationId, buffer, false);
  }
  readSpectrum(buffer: Float32Array): void {
    this.catalog.read(this.publicationId, buffer, true);
  }
  close(): void {
    this.bind(undefined, undefined);
    this.#listeners.clear();
  }
}

class ParticipantHandle implements RemoteParticipant {
  readonly videos = new Map<string, VideoHandle>();
  readonly audios = new Map<string, AudioHandle>();
  videoTracks: readonly RemoteVideoTrack[] = Object.freeze([]);
  audioTracks: readonly RemoteAudioTrack[] = Object.freeze([]);
  tracks: readonly RemotePublishedTrack[] = Object.freeze([]);
  get active(): boolean {
    return this.catalog.participants.includes(this);
  }
  id(): string {
    return this.participantId;
  }
  data<D extends Delivery>(label: string, delivery: D): RemoteDataTrack<D> {
    return this.catalog.data.remote(
      track.data(label, delivery),
      this.participantId,
    );
  }
  constructor(
    readonly catalog: RemoteCatalog,
    readonly participantId: string,
  ) {}
  video(label: string): VideoHandle {
    this.catalog.requireOpen();
    let handle = this.videos.get(label);
    if (!handle) {
      handle = new VideoHandle(this.catalog, this.participantId, label);
      this.videos.set(label, handle);
    }
    return handle;
  }
  audio(label: string): AudioHandle {
    this.catalog.requireOpen();
    let handle = this.audios.get(label);
    if (!handle) {
      handle = new AudioHandle(this.catalog, this.participantId, label);
      this.audios.set(label, handle);
    }
    return handle;
  }
}

export class RemoteCatalog implements RemoteMedia {
  readonly #handles = new Map<string, ParticipantHandle>();
  readonly #publicParticipants = new WeakMap<
    ParticipantHandle,
    RemoteParticipant
  >();
  readonly #publicVideos = new WeakMap<VideoHandle, RemoteVideoTrack>();
  readonly #publicAudios = new WeakMap<AudioHandle, RemoteAudioTrack>();
  readonly #publicCollections = new WeakMap<
    object,
    readonly RemotePublishedTrack[]
  >();
  readonly #publicParticipantCollections = new WeakMap<
    object,
    readonly RemoteParticipant[]
  >();
  #facade: RemoteMedia | undefined;
  #playback: RemoteAudioPlayback | undefined;
  #closed = false;
  participants: readonly RemoteParticipant[] = Object.freeze([]);
  videoTracks: readonly RemoteVideoTrack[] = Object.freeze([]);
  audioTracks: readonly RemoteAudioTrack[] = Object.freeze([]);
  constructor(
    readonly agent: Agent & MediaHost,
    readonly data: DataRegistry,
    readonly owners: ResourceOwners,
    private logLevel: LogLevel = "warn",
  ) {
    audioCatalogs.set(agent, this);
  }
  facade(): RemoteMedia {
    const thisCatalog = this;
    const wrapVideo = (handle: VideoHandle): RemoteVideoTrack => {
      let value = this.#publicVideos.get(handle);
      if (!value) {
        value = Object.freeze({
          kind: handle.kind,
          label: handle.label,
          participantId: handle.participantId,
          get id() {
            return handle.id;
          },
          get active() {
            return handle.active;
          },
          watch: (listener: () => void) => handle.watch(listener),
          subscribe: (configuration: VideoObservation) =>
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
          label: handle.label,
          participantId: handle.participantId,
          get id() {
            return handle.id;
          },
          get active() {
            return handle.active;
          },
          watch: (listener: () => void) => handle.watch(listener),
          subscribe: (configuration: AudioSubscriptionConfig) =>
            handle.subscribe(configuration),
        });
        this.#publicAudios.set(handle, value);
      }
      return value;
    };
    const wrapParticipant = (handle: ParticipantHandle): RemoteParticipant => {
      let value = this.#publicParticipants.get(handle);
      if (value) return value;
      const wrapTrack = (entry: RemotePublishedTrack): RemotePublishedTrack =>
        entry.kind === "video"
          ? wrapVideo(entry as VideoHandle)
          : entry.kind === "audio"
            ? wrapAudio(entry as AudioHandle)
            : entry;
      value = Object.freeze({
        participantId: handle.participantId,
        id: () => handle.id(),
        get active() {
          return handle.active;
        },
        get tracks() {
          let collection = thisCatalog.#publicCollections.get(
            handle.tracks as object,
          );
          if (!collection) {
            collection = Object.freeze(handle.tracks.map(wrapTrack));
            thisCatalog.#publicCollections.set(
              handle.tracks as object,
              collection,
            );
          }
          return collection;
        },
        video: (label: string) => wrapVideo(handle.video(label)),
        audio: (label: string) => wrapAudio(handle.audio(label)),
        data: <D extends Delivery>(label: string, delivery: D) =>
          handle.data(label, delivery),
      });
      this.#publicParticipants.set(handle, value);
      return value;
    };
    this.#facade ??= Object.freeze({
      get participants() {
        const source = thisCatalog.participants as readonly ParticipantHandle[];
        let collection = thisCatalog.#publicParticipantCollections.get(
          source as object,
        );
        if (!collection) {
          collection = Object.freeze(source.map(wrapParticipant));
          thisCatalog.#publicParticipantCollections.set(
            source as object,
            collection,
          );
        }
        return collection;
      },
      participant: (participantId: string) =>
        wrapParticipant(this.participant(participantId)),
    });
    return this.#facade;
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
        bind: () => {
          playback = new RemoteAudioPlayback(this.logLevel, report);
          this.#playback = playback;
          playback.start(this.agent.getSnapshot());
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
  participant(participantId: string): ParticipantHandle {
    this.requireOpen();
    let handle = this.#handles.get(participantId);
    if (!handle) {
      handle = new ParticipantHandle(this, participantId);
      this.#handles.set(participantId, handle);
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
  read(id: string | undefined, buffer: Float32Array, spectrum: boolean): void {
    if (this.#playback) this.#playback.read(id, buffer, spectrum);
    else buffer.fill(spectrum ? -Infinity : 0);
  }
  update(snapshot: AgentSnapshot): void {
    if (this.#closed) return;
    this.#playback?.update(snapshot);
    const byId = new Map<string, ParticipantHandle>();
    const participants: ParticipantHandle[] = [];
    for (const participant of snapshot.catalog.participants) {
      if (participant.participantId === snapshot.participantId) continue;
      const handle = this.participant(participant.participantId);
      byId.set(participant.participantId, handle);
      participants.push(handle);
    }
    const videoIds = new Map<VideoHandle, string>();
    const audioIds = new Map<AudioHandle, string>();
    const videos: VideoHandle[] = [];
    const audios: AudioHandle[] = [];
    for (const publication of snapshot.catalog.publications) {
      const participant = byId.get(publication.participantId);
      if (!participant) continue;
      if (publication.kind === "video") {
        const handle = participant.video(publication.label);
        videoIds.set(handle, publication.id);
        videos.push(handle);
      } else {
        const handle = participant.audio(publication.label);
        audioIds.set(handle, publication.id);
        audios.push(handle);
      }
    }
    const receivers = new Map(
      snapshot.mapping.audio.map(({ publicationId, receiverIndex }) => [
        publicationId,
        receiverIndex,
      ]),
    );
    for (const participant of this.#handles.values()) {
      for (const handle of participant.videos.values())
        handle.bind(videoIds.get(handle));
      for (const handle of participant.audios.values()) {
        const id = audioIds.get(handle);
        handle.bind(id, id === undefined ? undefined : receivers.get(id));
      }
      participant.tracks = collection(
        participant.tracks,
        snapshot.catalog.tracks
          .filter((entry) => entry.participantId === participant.participantId)
          .map((entry) =>
            entry.kind === "data"
              ? participant.data(entry.label, entry.delivery)
              : entry.kind === "video"
                ? participant.video(entry.label)
                : participant.audio(entry.label),
          ),
      );
      participant.videoTracks = collection(
        participant.videoTracks,
        videos.filter(
          (handle) => handle.participantId === participant.participantId,
        ),
      );
      participant.audioTracks = collection(
        participant.audioTracks,
        audios.filter(
          (handle) => handle.participantId === participant.participantId,
        ),
      );
    }
    this.participants = collection(this.participants, participants);
    this.videoTracks = collection(this.videoTracks, videos);
    this.audioTracks = collection(this.audioTracks, audios);
  }
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.owners.close();
    this.#playback?.close();
    this.#playback = undefined;
    audioCatalogs.delete(this.agent);
    for (const participant of this.#handles.values()) {
      for (const handle of participant.videos.values()) handle.close();
      for (const handle of participant.audios.values()) handle.close();
      participant.videoTracks = Object.freeze([]);
      participant.audioTracks = Object.freeze([]);
      participant.tracks = Object.freeze([]);
    }
    this.participants = Object.freeze([]);
    this.videoTracks = Object.freeze([]);
    this.audioTracks = Object.freeze([]);
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

function visibleInTree(element: HTMLElement): boolean {
  if (element.ownerDocument.hidden) return false;
  for (
    let node: HTMLElement | null = element;
    node;
    node = node.parentElement
  ) {
    const style = node.ownerDocument.defaultView?.getComputedStyle(node);
    if (
      node.hidden ||
      style?.display === "none" ||
      style?.visibility === "hidden" ||
      style?.visibility === "collapse" ||
      Number(style?.opacity || "1") === 0
    )
      return false;
  }
  return true;
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
  let preferences = validateReceiveOptions(options.preferences ?? {});
  const claim = claimVideoSubscription(
    handle.catalog.owners,
    {
      participantId: handle.participantId,
      label: handle.label,
    },
    {
      visible: false,
      height: 0,
      minHeight: 0,
      minFps: 0,
      priority: 0,
      playoutDelay: { mode: "adaptive" },
    },
  );
  const subscription = claim.subscription;
  const agent = handle.catalog.agent;
  const attachment = attachRemoteMedia(subscription, element, {
    ready: claim.ready,
    subscribe: agent.subscribe,
    onPlaybackBlocked,
    autoPlay: options.autoPlay,
  });
  const viewport = element.ownerDocument.defaultView;
  let intersecting = true;
  let closed = false;
  const sync = () => {
    if (closed) return;
    const rect = element.getBoundingClientRect();
    const visible =
      element.isConnected &&
      intersecting &&
      visibleInTree(element) &&
      rect.width > 0 &&
      rect.height > 0 &&
      (viewport === null ||
        (rect.bottom > 0 &&
          rect.right > 0 &&
          rect.top < viewport.innerHeight &&
          rect.left < viewport.innerWidth));
    const height = visible
      ? Math.min(
          MAX_UINT32,
          Math.ceil(rect.height * (viewport?.devicePixelRatio || 1)),
        )
      : 0;
    if (subscription.isClosed()) return;
    const observation: VideoObservation = {
      visible,
      height,
      minHeight: preferences.minHeight ?? 0,
      minFps: preferences.minFps ?? 0,
      priority: preferences.priority ?? 0,
      playoutDelay: preferences.playoutDelay
        ? { mode: "fixed", ...preferences.playoutDelay }
        : { mode: "adaptive" },
    };
    void subscription.configure(observation).catch((error: unknown) => {
      if (closed || subscription.isClosed()) return;
      const retry = () => subscription.configure(observation);
      onPlaybackBlocked?.(
        {
          message: error instanceof Error ? error.message : String(error),
          cause: error,
          retry,
        },
        retry,
      );
    });
  };
  let pixelRatioQuery: MediaQueryList | null = null;
  const onPixelRatioChange = () => {
    watchPixelRatio();
    sync();
  };
  const watchPixelRatio = () => {
    pixelRatioQuery?.removeEventListener("change", onPixelRatioChange);
    pixelRatioQuery =
      viewport?.matchMedia?.(
        `(resolution: ${viewport.devicePixelRatio || 1}dppx)`,
      ) ?? null;
    pixelRatioQuery?.addEventListener("change", onPixelRatioChange);
  };
  watchPixelRatio();
  const resize =
    typeof ResizeObserver === "undefined" ? null : new ResizeObserver(sync);
  resize?.observe(element);
  const intersection =
    typeof IntersectionObserver === "undefined"
      ? null
      : new IntersectionObserver(([entry]) => {
          intersecting = entry?.isIntersecting ?? false;
          sync();
        });
  intersection?.observe(element);
  const mutation = new MutationObserver(sync);
  mutation.observe(element.ownerDocument.documentElement, {
    attributes: true,
    attributeFilter: ["style", "class", "hidden"],
    childList: true,
    subtree: true,
  });
  const onResize = () => {
    watchPixelRatio();
    sync();
  };
  viewport?.addEventListener("resize", onResize);
  viewport?.addEventListener("scroll", sync, true);
  element.ownerDocument.addEventListener("visibilitychange", sync);
  sync();
  return {
    ready: claim.ready,
    setAutoPlay: (autoPlay) => attachment.setAutoPlay(autoPlay),
    setPreferences: (next) => {
      if (closed) return;
      preferences = validateReceiveOptions(next);
      sync();
    },
    retryPlayback: () => attachment.retryPlayback(),
    close: () => {
      if (closed) return attachment.close();
      closed = true;
      resize?.disconnect();
      intersection?.disconnect();
      mutation.disconnect();
      pixelRatioQuery?.removeEventListener("change", onPixelRatioChange);
      viewport?.removeEventListener("resize", onResize);
      viewport?.removeEventListener("scroll", sync, true);
      element.ownerDocument.removeEventListener("visibilitychange", sync);
      return attachment.close();
    },
  };
}
