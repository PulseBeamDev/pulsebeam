import { attachRemoteMedia } from "./remote-media.js";
import { RemoteAudioPlayback } from "./remote-audio.js";
import type {
  Agent,
  AgentSnapshot,
  LogLevel,
  PlaybackFailure,
  ReceiveOptions,
  RemoteAudioTrack,
  RemoteParticipant,
  RemoteMediaAttachment,
  RemoteMediaAttachmentOptions,
  RemoteVideoTrack,
  RemoteMedia,
  VideoDemand,
} from "./types.js";

type PlaybackCallback = (
  failure: PlaybackFailure,
  retry: () => Promise<void>,
) => void;
type Demand = { height: number; visible: boolean };
const MAX_UINT32 = 0xffff_ffff;
const videoOwners = new WeakMap<RemoteVideoTrack, VideoHandle>();

function uint32(value: number, field: string): number {
  if (!Number.isInteger(value) || value < 0 || value > MAX_UINT32)
    throw new TypeError(`${field} must be a finite uint32 integer`);
  return value;
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
  readonly consumers = new Map<symbol, Demand>();
  readonly bindingListeners = new Set<() => void>();
  readonly #listeners = new Set<() => void>();
  options: ReceiveOptions = receiveOptions({});
  constructor(
    readonly catalog: RemoteCatalog,
    readonly participantId: string,
    readonly label: string,
  ) {
    videoOwners.set(this, this);
  }
  get active(): boolean {
    return this.publicationId !== undefined;
  }
  subscribe(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }
  bind(id: string | undefined): void {
    if (id === this.publicationId) return;
    this.publicationId = id;
    for (const listener of this.bindingListeners) listener();
    for (const listener of this.#listeners) listener();
  }
  setReceiveOptions(options: ReceiveOptions): void {
    if (this.catalog.closed) return;
    const next = receiveOptions(options);
    if (JSON.stringify(this.options) === JSON.stringify(next)) return;
    this.options = next;
    this.catalog.recompute();
  }
  consume(token: symbol, height: number, visible: boolean): void {
    if (this.catalog.closed) return;
    const prior = this.consumers.get(token);
    if (prior?.height === height && prior.visible === visible) return;
    this.consumers.set(token, { height, visible });
    this.catalog.recompute();
  }
  release(token: symbol): void {
    if (this.consumers.delete(token)) this.catalog.recompute();
  }
  close(): void {
    this.bind(undefined);
    this.consumers.clear();
    this.bindingListeners.clear();
    this.#listeners.clear();
  }
  get height(): number {
    return Math.max(
      0,
      ...[...this.consumers.values()]
        .filter(({ visible }) => visible)
        .map(({ height }) => height),
    );
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
  get receiving(): boolean {
    return this.#binding !== undefined;
  }
  bind(id: string | undefined, receiver: number | undefined): void {
    const changed = id !== this.publicationId || receiver !== this.#binding;
    this.publicationId = id;
    this.#binding = receiver;
    if (changed) for (const listener of this.#listeners) listener();
  }
  subscribe(listener: () => void): () => void {
    if (this.catalog.closed) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
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
  constructor(
    readonly catalog: RemoteCatalog,
    readonly externalId: string,
  ) {}
  video(label: string): VideoHandle {
    this.catalog.requireOpen();
    let handle = this.videos.get(label);
    if (!handle) {
      handle = new VideoHandle(this.catalog, this.externalId, label);
      this.videos.set(label, handle);
    }
    return handle;
  }
  audio(label: string): AudioHandle {
    this.catalog.requireOpen();
    let handle = this.audios.get(label);
    if (!handle) {
      handle = new AudioHandle(this.catalog, this.externalId, label);
      this.audios.set(label, handle);
    }
    return handle;
  }
}

export class RemoteCatalog implements RemoteMedia {
  readonly #handles = new Map<string, ParticipantHandle>();
  readonly #slots = new Map<VideoHandle, number>();
  readonly #warned = new Set<VideoHandle>();
  readonly #playback: RemoteAudioPlayback;
  #closed = false;
  participants: readonly RemoteParticipant[] = Object.freeze([]);
  videoTracks: readonly RemoteVideoTrack[] = Object.freeze([]);
  audioTracks: readonly RemoteAudioTrack[] = Object.freeze([]);
  constructor(
    readonly agent: Agent,
    private readonly capacity: number,
    private readonly onChange: (video: readonly VideoDemand[]) => void,
    logLevel: LogLevel = "warn",
  ) {
    this.#playback = new RemoteAudioPlayback(logLevel);
  }
  get closed(): boolean {
    return this.#closed;
  }
  requireOpen(): void {
    if (this.#closed) throw new Error("agent is closed");
  }
  participant(externalId: string): ParticipantHandle {
    this.requireOpen();
    let handle = this.#handles.get(externalId);
    if (!handle) {
      handle = new ParticipantHandle(this, externalId);
      this.#handles.set(externalId, handle);
    }
    return handle;
  }
  resumeAudio(): Promise<void> {
    return this.#playback.resume();
  }
  read(id: string | undefined, buffer: Float32Array, spectrum: boolean): void {
    this.#playback.read(id, buffer, spectrum);
  }
  update(snapshot: AgentSnapshot): void {
    if (this.#closed) return;
    this.#playback.update(snapshot);
    const byId = new Map<string, ParticipantHandle>();
    const participants: ParticipantHandle[] = [];
    for (const participant of snapshot.catalog.participants) {
      if (
        participant.id === snapshot.participantId ||
        participant.externalId === snapshot.participantExternalId
      )
        continue;
      const handle = this.participant(participant.externalId);
      byId.set(participant.id, handle);
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
      participant.videoTracks = collection(
        participant.videoTracks,
        videos.filter(
          (handle) => handle.participantId === participant.externalId,
        ),
      );
      participant.audioTracks = collection(
        participant.audioTracks,
        audios.filter(
          (handle) => handle.participantId === participant.externalId,
        ),
      );
    }
    this.participants = collection(this.participants, participants);
    this.videoTracks = collection(this.videoTracks, videos);
    this.audioTracks = collection(this.audioTracks, audios);
    this.recompute();
  }
  recompute(): void {
    if (this.#closed) return;
    const candidates = (this.videoTracks as readonly VideoHandle[]).filter(
      (handle) => handle.height > 0,
    );
    const selected = new Map<VideoHandle, number>();
    const occupied = new Set<number>();
    for (const handle of candidates) {
      const slot = this.#slots.get(handle);
      if (slot !== undefined && slot < this.capacity && !occupied.has(slot)) {
        selected.set(handle, slot);
        occupied.add(slot);
      }
    }
    for (const handle of candidates) {
      if (selected.has(handle)) continue;
      for (let slot = 0; slot < this.capacity; slot++) {
        if (occupied.has(slot)) continue;
        selected.set(handle, slot);
        occupied.add(slot);
        break;
      }
    }
    this.#slots.clear();
    for (const [handle, slot] of selected) this.#slots.set(handle, slot);
    for (const handle of candidates) {
      if (!selected.has(handle) && !this.#warned.has(handle)) {
        console.warn(
          "PulseBeam remote video demand exceeds receiver capacity",
          {
            participantId: handle.participantId,
            label: handle.label,
            capacity: this.capacity,
          },
        );
        this.#warned.add(handle);
      }
      if (selected.has(handle)) this.#warned.delete(handle);
    }
    this.onChange(
      [...selected]
        .sort(([, a], [, b]) => a - b)
        .map(([handle, slot]) => ({
          slot,
          trackId: handle.publicationId!,
          height: uint32(handle.height, "height"),
          minHeight: handle.options.minHeight ?? 0,
          minFps: handle.options.minFps ?? 0,
          priority: handle.options.priority ?? 0,
          ...(handle.options.playoutDelay
            ? {
                playoutDelay: {
                  mode: "fixed" as const,
                  ...handle.options.playoutDelay,
                },
              }
            : {}),
        })),
    );
  }
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#playback.close();
    for (const participant of this.#handles.values()) {
      for (const handle of participant.videos.values()) handle.close();
      for (const handle of participant.audios.values()) handle.close();
      participant.videoTracks = Object.freeze([]);
      participant.audioTracks = Object.freeze([]);
    }
    this.#slots.clear();
    this.#warned.clear();
    this.participants = Object.freeze([]);
    this.videoTracks = Object.freeze([]);
    this.audioTracks = Object.freeze([]);
  }
}

export function attachRemoteVideo(
  source: RemoteVideoTrack,
  element: HTMLVideoElement,
  onPlaybackBlocked?: PlaybackCallback,
  options: Pick<RemoteMediaAttachmentOptions, "autoPlay"> = {},
): RemoteMediaAttachment {
  const handle = videoOwners.get(source);
  if (!handle) throw new TypeError("invalid remote video source");
  if (handle.catalog.closed)
    return {
      setPublicationIds() {},
      retryPlayback: async () => {},
      close() {},
    };
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
      Number(style?.opacity ?? 1) === 0
    )
      return false;
  }
  return true;
}

function attachVideoHandle(
  handle: VideoHandle,
  element: HTMLVideoElement,
  onPlaybackBlocked: PlaybackCallback | undefined,
  options: Pick<RemoteMediaAttachmentOptions, "autoPlay">,
): RemoteMediaAttachment {
  const agent = handle.catalog.agent;
  const attachment = attachRemoteMedia(agent, element, {
    publicationIds: handle.publicationId ? [handle.publicationId] : [],
    onPlaybackBlocked,
    autoPlay: options.autoPlay,
  });
  const token = Symbol("video-consumer");
  const viewport = element.ownerDocument.defaultView;
  let intersecting = true;
  let closed = false;
  const sync = () => {
    if (closed) return;
    const rect = element.getBoundingClientRect();
    const visible =
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
    handle.consume(token, height, visible);
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
  for (let node: HTMLElement | null = element; node; node = node.parentElement)
    mutation.observe(node, {
      attributes: true,
      attributeFilter: ["style", "class", "hidden"],
    });
  const onResize = () => {
    watchPixelRatio();
    sync();
  };
  viewport?.addEventListener("resize", onResize);
  viewport?.addEventListener("scroll", sync, true);
  element.ownerDocument.addEventListener("visibilitychange", sync);
  const updateBinding = () =>
    attachment.setPublicationIds(
      handle.publicationId ? [handle.publicationId] : [],
    );
  handle.bindingListeners.add(updateBinding);
  sync();
  return {
    setPublicationIds: (ids) => attachment.setPublicationIds(ids),
    retryPlayback: () => attachment.retryPlayback(),
    close: () => {
      if (closed) return;
      closed = true;
      resize?.disconnect();
      intersection?.disconnect();
      mutation.disconnect();
      pixelRatioQuery?.removeEventListener("change", onPixelRatioChange);
      viewport?.removeEventListener("resize", onResize);
      viewport?.removeEventListener("scroll", sync, true);
      element.ownerDocument.removeEventListener("visibilitychange", sync);
      handle.bindingListeners.delete(updateBinding);
      handle.release(token);
      attachment.close();
    },
  };
}
