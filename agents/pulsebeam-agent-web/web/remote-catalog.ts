import { attachRemoteMedia } from "./remote-media.js";
import type {
  Agent,
  AgentSnapshot,
  PlaybackFailure,
  ReceiveOptions,
  RemoteAudioSource,
  RemoteAudioTrack,
  RemoteMediaAttachment,
  RemoteMediaAttachmentOptions,
  RemoteVideoTrack,
  VideoDemand,
} from "./types.js";

type PlaybackCallback = (
  failure: PlaybackFailure,
  retry: () => Promise<void>,
) => void;
type Demand = { height: number; visible: boolean };
const MAX_UINT32 = 0xffff_ffff;
const videoOwners = new WeakMap<RemoteVideoTrack, VideoHandle>();
const audioOwners = new WeakMap<RemoteAudioSource, RemoteCatalog>();

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

class VideoHandle implements RemoteVideoTrack {
  readonly kind = "video" as const;
  readonly participantId: string;
  readonly label: string;
  readonly publicationId: string;
  readonly #catalog: RemoteCatalog;
  readonly consumers = new Map<symbol, Demand>();
  readonly retireListeners = new Set<() => void>();
  readonly #listeners = new Set<() => void>();
  options: ReceiveOptions = receiveOptions({});
  retired = false;

  constructor(
    catalog: RemoteCatalog,
    id: string,
    participantId: string,
    label: string,
  ) {
    this.#catalog = catalog;
    this.publicationId = id;
    this.participantId = participantId;
    this.label = label;
    videoOwners.set(this, this);
  }

  get catalog(): RemoteCatalog {
    return this.#catalog;
  }

  get active(): boolean {
    return !this.retired;
  }

  subscribe(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  setReceiveOptions(options: ReceiveOptions): void {
    if (this.retired) return;
    const next = receiveOptions(options);
    if (JSON.stringify(this.options) === JSON.stringify(next)) return;
    this.options = next;
    this.#catalog.recompute();
  }

  consume(token: symbol, height: number, visible: boolean): void {
    if (this.retired) return;
    const prior = this.consumers.get(token);
    if (prior?.height === height && prior.visible === visible) return;
    this.consumers.set(token, { height, visible });
    this.#catalog.recompute();
  }

  release(token: symbol): void {
    if (this.consumers.delete(token)) this.#catalog.recompute();
  }

  retire(): void {
    if (this.retired) return;
    this.retired = true;
    this.consumers.clear();
    this.options = receiveOptions({});
    for (const listener of this.retireListeners) listener();
    this.retireListeners.clear();
    for (const listener of this.#listeners) listener();
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
  readonly participantId: string;
  readonly label: string;
  readonly publicationId: string;
  constructor(id: string, participantId: string, label: string) {
    this.publicationId = id;
    this.participantId = participantId;
    this.label = label;
  }
}

export class RemoteCatalog {
  readonly #agent: Agent;
  readonly #capacity: number;
  readonly #onChange: (video: readonly VideoDemand[], audio: boolean) => void;
  readonly #videos = new Map<string, VideoHandle>();
  readonly #audios = new Map<string, AudioHandle>();
  readonly #slots = new Map<VideoHandle, number>();
  readonly #warned = new Set<VideoHandle>();
  #audioConsumers = 0;
  #closed = false;
  videoTracks: readonly RemoteVideoTrack[] = Object.freeze([]);
  audioTracks: readonly RemoteAudioTrack[] = Object.freeze([]);
  readonly audioSource: RemoteAudioSource = Object.freeze({
    kind: "remote-audio",
  });

  constructor(
    agent: Agent,
    capacity: number,
    onChange: (video: readonly VideoDemand[], audio: boolean) => void,
  ) {
    this.#agent = agent;
    this.#capacity = capacity;
    this.#onChange = onChange;
    audioOwners.set(this.audioSource, this);
  }

  get agent(): Agent {
    return this.#agent;
  }

  update(snapshot: AgentSnapshot): void {
    if (this.#closed) return;
    const participants = new Map(
      snapshot.catalog.participants.map(({ id, externalId }) => [
        id,
        externalId,
      ]),
    );
    const videos = new Set<string>();
    const audios = new Set<string>();
    let changed = false;
    for (const publication of snapshot.catalog.publications) {
      const participant = participants.get(publication.participantId);
      if (participant === undefined) continue;
      const { id, label } = publication;
      if (publication.kind === "video") {
        videos.add(id);
        const prior = this.#videos.get(id);
        if (
          prior &&
          prior.participantId === participant &&
          prior.label === label
        )
          continue;
        if (prior) prior.retire();
        this.#videos.set(id, new VideoHandle(this, id, participant, label));
        changed = true;
      } else {
        audios.add(id);
        const prior = this.#audios.get(id);
        if (
          prior &&
          prior.participantId === participant &&
          prior.label === label
        )
          continue;
        this.#audios.set(id, new AudioHandle(id, participant, label));
        changed = true;
      }
    }
    for (const [id, handle] of this.#videos) {
      if (videos.has(id)) continue;
      handle.retire();
      this.#videos.delete(id);
      changed = true;
    }
    for (const id of this.#audios.keys()) {
      if (audios.has(id)) continue;
      this.#audios.delete(id);
      changed = true;
    }
    if (!changed) return;
    this.videoTracks = Object.freeze([...this.#videos.values()]);
    this.audioTracks = Object.freeze([...this.#audios.values()]);
    this.recompute();
  }

  recompute(): void {
    if (this.#closed) return;
    const candidates = [...this.#videos.values()].filter(
      (handle) => handle.height > 0,
    );
    const selected = new Map<VideoHandle, number>();
    const occupied = new Set<number>();
    for (const handle of candidates) {
      const slot = this.#slots.get(handle);
      if (slot !== undefined && slot < this.#capacity && !occupied.has(slot)) {
        selected.set(handle, slot);
        occupied.add(slot);
      }
    }
    for (const handle of candidates) {
      if (selected.has(handle)) continue;
      for (let slot = 0; slot < this.#capacity; slot++) {
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
            capacity: this.#capacity,
          },
        );
        this.#warned.add(handle);
      }
      if (selected.has(handle)) this.#warned.delete(handle);
    }
    const video = [...selected]
      .sort(([, a], [, b]) => a - b)
      .map(([handle, slot]) => ({
        slot,
        trackId: handle.publicationId,
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
      }));
    this.#onChange(video, this.#audioConsumers > 0);
  }

  addAudioConsumer(): () => void {
    if (this.#closed) return () => {};
    this.#audioConsumers += 1;
    if (this.#audioConsumers === 1) this.recompute();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.#audioConsumers -= 1;
      if (this.#audioConsumers === 0) this.recompute();
    };
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const handle of this.#videos.values()) handle.retire();
    this.#videos.clear();
    this.#audios.clear();
    this.#slots.clear();
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
  if (handle.retired)
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
    publicationIds: [handle.publicationId],
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
  const retire = () => attachment.setPublicationIds([]);
  handle.retireListeners.add(retire);
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
      handle.retireListeners.delete(retire);
      handle.release(token);
      attachment.close();
    },
  };
}

export function attachRemoteAudio(
  source: RemoteAudioSource,
  element: HTMLAudioElement,
  onPlaybackBlocked?: PlaybackCallback,
  options: Pick<RemoteMediaAttachmentOptions, "autoPlay"> = {},
): RemoteMediaAttachment {
  const catalog = audioOwners.get(source);
  if (!catalog) throw new TypeError("invalid remote audio source");
  const release = catalog.addAudioConsumer();
  const agent = catalog.agent;
  const attachment = attachRemoteMedia(agent, element, {
    publicationIds: [],
    onPlaybackBlocked,
    autoPlay: options.autoPlay,
  });
  const update = () => {
    const current = new Set(
      catalog.audioTracks.map((track) => (track as AudioHandle).publicationId),
    );
    attachment.setPublicationIds(
      agent
        .getSnapshot()
        .mapping.audio.map(({ publicationId }) => publicationId)
        .filter((id) => current.has(id)),
    );
  };
  const unsubscribe = agent.subscribe(update);
  update();
  let closed = false;
  return {
    setPublicationIds: (ids) => attachment.setPublicationIds(ids),
    retryPlayback: () => attachment.retryPlayback(),
    close: () => {
      if (closed) return;
      closed = true;
      unsubscribe();
      attachment.close();
      release();
    },
  };
}
