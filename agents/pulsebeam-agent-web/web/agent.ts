import { senderConfig } from "./presets.js";
import { TopicController, type TopicOwner } from "./topic.js";
import type {
  Agent,
  AgentConfig,
  AgentFailure,
  AgentSnapshot,
  AgentStatistics,
  AudioDemand,
  AudioPolicy,
  ConnectionState,
  LocalPublication,
  MediaKind,
  PlayoutDelay,
  Publication,
  RemoteAudio,
  RemoteVideo,
  SenderEncodingStatistics,
  SenderStatistics,
  Topic,
  TopicDelivery,
  TopicMode,
  TopicOptions,
  TopicState,
  VideoDemand,
  VideoPolicy,
} from "./types.js";
import { PulseBeamError } from "./types.js";
import { createRuntime, type RuntimeHost } from "./wasm.js";

interface NormalizedConfig {
  readonly endpoint: string;
  readonly roomId: string;
  readonly requestHeaders: Readonly<Record<string, string>>;
  readonly topology: Readonly<{
    localVideo: readonly string[];
    localAudio: readonly string[];
    remoteVideo: number;
    remoteAudio: number;
  }>;
}

interface RawFailure {
  class: AgentFailure["class"];
  message: string;
}

interface RawPublication {
  id: string;
  participantId: string;
  kind: MediaKind;
}

interface RawVideoBinding {
  trackId: string;
  mid: string;
  paused: boolean;
}

interface RawAudioBinding {
  trackId: string;
  mid: string;
  levelDbov: number;
}

interface RawTopicState extends TopicState {}

interface RawSnapshot {
  version: bigint;
  desiredRevision: bigint;
  connection: ConnectionState;
  generation?: bigint;
  participantId?: string;
  participants: Array<{ id: string }>;
  publications: RawPublication[];
  video: RawVideoBinding[];
  audio: RawAudioBinding[];
  topics: RawTopicState;
  failure?: RawFailure;
}

type RawEvent = Readonly<Record<string, unknown>>;

interface LocalState {
  readonly kind: MediaKind;
  readonly track: MediaStreamTrack;
  readonly policy: VideoPolicy | AudioPolicy;
  readonly muted: boolean;
}

interface RemoteResource {
  readonly stream: MediaStream;
  track?: MediaStreamTrack;
}

interface DesiredModel {
  connected: boolean;
  video: readonly VideoDemand[];
  audio: Readonly<{ pinned: readonly string[]; automatic: boolean }>;
  playoutDelay: PlayoutDelay;
}

const EMPTY_TOPICS: TopicState = Object.freeze({
  publishers: Object.freeze([]),
  subscribers: Object.freeze([]),
  acceptedSends: 0n,
  droppedSends: 0n,
  deliveredMessages: 0n,
  resynchronizations: 0n,
  channelFailures: 0n,
});

export async function createAgent(config: AgentConfig): Promise<Agent> {
  const normalized = normalizeConfig(config);
  try {
    const runtime = await createRuntime(normalized);
    return new BrowserAgent(runtime, normalized);
  } catch (error) {
    if (error instanceof PulseBeamError) throw error;
    throw new PulseBeamError("invalid-config", errorMessage(error), error);
  }
}

class BrowserAgent implements Agent, TopicOwner {
  readonly #runtime: RuntimeHost;
  readonly #config: NormalizedConfig;
  readonly #listeners = new Set<() => void>();
  readonly #topics = new Map<string, TopicController>();
  readonly #local = new Map<string, LocalState>();
  readonly #remote = new Map<string, RemoteResource>();
  #desired: DesiredModel = {
    connected: false,
    video: Object.freeze([]),
    audio: Object.freeze({ pinned: Object.freeze([]), automatic: true }),
    playoutDelay: Object.freeze({ mode: "adaptive" }),
  };
  #raw: RawSnapshot;
  #snapshot: AgentSnapshot;
  #projectionKey = "";
  #localVersion = 0;
  #mediaWork = Promise.resolve();
  #failure: AgentFailure | undefined;
  #closed = false;
  #closing = false;
  #closePromise: Promise<void> | undefined;
  #resolveClose: (() => void) | undefined;

  constructor(runtime: RuntimeHost, config: NormalizedConfig) {
    this.#runtime = runtime;
    this.#config = config;
    this.#raw = runtime.snapshot() as RawSnapshot;
    this.#snapshot = this.#buildSnapshot();
    runtime.set_snapshot_listener((snapshot) => this.#acceptSnapshot(snapshot as RawSnapshot));
    runtime.set_event_listener((event) => this.#acceptEvent(event as RawEvent));
    runtime.set_error_listener((message) => {
      this.#failure = Object.freeze({ class: "browser", message });
      this.#localVersion += 1;
      this.#publish();
    });
  }

  getSnapshot = (): AgentSnapshot => this.#snapshot;

  subscribe = (listener: () => void): (() => void) => {
    if (this.#closed) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  connect(): void {
    this.#ensureOpen();
    if (this.#desired.connected) return;
    this.#desired = { ...this.#desired, connected: true };
    this.#submitDesired();
  }

  reconnect(): void {
    this.#ensureOpen();
    try {
      this.#runtime.force_reconnect();
    } catch (error) {
      throw commandError("could not reconnect", error);
    }
  }

  close(): Promise<void> {
    if (this.#closed) return Promise.resolve();
    if (this.#closePromise) return this.#closePromise;
    this.#closing = true;
    this.#closePromise = new Promise((resolve) => {
      this.#resolveClose = resolve;
    });
    if (this.#raw.connection === "disconnected" || this.#raw.connection === "terminal-failure") {
      this.#finishClose();
      return this.#closePromise;
    }
    this.#desired = { ...this.#desired, connected: false };
    this.#submitDesired();
    return this.#closePromise;
  }

  abort(): void {
    if (this.#closed) return;
    this.#closing = true;
    this.#finishClose();
  }

  setLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    policy?: VideoPolicy | AudioPolicy,
  ): Promise<void> {
    this.#ensureOpen();
    return this.#queueMedia(() => this.#replaceLocalTrack(slot, track, policy));
  }

  setLocalMuted(slot: string, muted: boolean): Promise<void> {
    this.#ensureOpen();
    return this.#queueMedia(() => this.#replaceLocalMuted(slot, muted));
  }

  async #replaceLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    policy?: VideoPolicy | AudioPolicy,
  ): Promise<void> {
    this.#ensureOpen();
    const kind = this.#localKind(slot);
    if (track && track.kind !== kind) {
      throw new PulseBeamError(
        "invalid-command",
        `local slot ${slot} requires a ${kind} track`,
      );
    }
    const resolvedPolicy = resolvePolicy(kind, policy);
    if (track && kind === "audio") {
      const channels = resolvedPolicy === "music" ? 2 : 1;
      await track.applyConstraints({ channelCount: { ideal: channels } }).catch(() => {});
    }
    try {
      await this.#runtime.replace_local_track(
        slot,
        track,
        senderConfig(kind, resolvedPolicy),
      );
    } catch (error) {
      throw commandError(`could not replace local track in slot ${slot}`, error);
    }
    if (track) {
      const muted = this.#local.get(slot)?.muted ?? false;
      this.#local.set(slot, { kind, track, policy: resolvedPolicy, muted });
    } else {
      this.#local.delete(slot);
    }
    this.#localVersion += 1;
    this.#submitDesired();
  }

  async #replaceLocalMuted(slot: string, muted: boolean): Promise<void> {
    this.#ensureOpen();
    const state = this.#local.get(slot);
    if (!state) {
      throw new PulseBeamError("invalid-command", `local slot has no track: ${slot}`);
    }
    if (state.muted === muted) return;
    try {
      await this.#runtime.set_local_muted(slot, muted);
    } catch (error) {
      throw commandError(`could not update local mute for slot ${slot}`, error);
    }
    state.track.enabled = !muted;
    this.#local.set(slot, { ...state, muted });
    this.#localVersion += 1;
    this.#submitDesired();
  }

  setVideoDemand(demand: readonly VideoDemand[]): void {
    this.#ensureOpen();
    const normalized = normalizeVideoDemand(demand, this.#config.topology.remoteVideo);
    if (sameJson(this.#desired.video, normalized)) return;
    this.#desired = { ...this.#desired, video: normalized };
    this.#submitDesired();
  }

  setAudioDemand(demand: AudioDemand): void {
    this.#ensureOpen();
    const pinned = Object.freeze([...(demand.pinned ?? [])]);
    if (new Set(pinned).size !== pinned.length || pinned.some((id) => !validIdentifier(id, true))) {
      throw new PulseBeamError("invalid-command", "audio pins must be unique publication IDs");
    }
    const audio = Object.freeze({ pinned, automatic: demand.automatic ?? true });
    if (sameJson(this.#desired.audio, audio)) return;
    this.#desired = { ...this.#desired, audio };
    this.#submitDesired();
  }

  setPlayoutDelay(delay: PlayoutDelay): void {
    this.#ensureOpen();
    const normalized = normalizePlayoutDelay(delay);
    if (sameJson(this.#desired.playoutDelay, normalized)) return;
    this.#desired = { ...this.#desired, playoutDelay: normalized };
    this.#submitDesired();
  }

  openTopic(options: TopicOptions): Topic {
    this.#ensureOpen();
    const normalized = normalizeTopicOptions(options);
    const key = topicKey(normalized.name, normalized.mode);
    const existing = this.#topics.get(key);
    if (existing) {
      if (!sameJson(existing.options, normalized)) {
        throw new PulseBeamError(
          "invalid-command",
          `topic ${normalized.name}/${normalized.mode} is already registered differently`,
        );
      }
      return existing;
    }
    const topic = new TopicController(this, normalized);
    this.#topics.set(key, topic);
    try {
      this.#submitDesired();
    } catch (error) {
      this.#topics.delete(key);
      topic.detach();
      throw error;
    }
    return topic;
  }

  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): void {
    this.#ensureOpen();
    if (!(payload instanceof Uint8Array)) {
      throw new PulseBeamError("invalid-command", "topic payload must be a Uint8Array");
    }
    try {
      this.#runtime.send_topic(name, mode, payload);
    } catch (error) {
      throw commandError(`could not send topic ${name}`, error);
    }
  }

  closeTopic(topic: TopicController): void {
    const key = topicKey(topic.name, topic.mode);
    if (this.#topics.get(key) !== topic) return;
    this.#topics.delete(key);
    topic.detach();
    if (!this.#closed && !this.#closing) this.#submitDesired();
  }

  async statistics(): Promise<AgentStatistics> {
    this.#ensureOpen();
    try {
      return projectStatistics((await this.#runtime.statistics()) as RawStatistics);
    } catch (error) {
      throw commandError("could not query browser statistics", error);
    }
  }

  #queueMedia(task: () => Promise<void>): Promise<void> {
    const work = this.#mediaWork.then(task);
    this.#mediaWork = work.catch(() => {});
    return work;
  }

  #ensureOpen(): void {
    if (this.#closed || this.#closing) {
      throw new PulseBeamError("closed", "agent is closed");
    }
  }

  #localKind(slot: string): MediaKind {
    if (this.#config.topology.localVideo.includes(slot)) return "video";
    if (this.#config.topology.localAudio.includes(slot)) return "audio";
    throw new PulseBeamError("invalid-command", `unknown local publication slot: ${slot}`);
  }

  #submitDesired(): void {
    const topics = [...this.#topics.values()].map((topic) => topic.options);
    const publications = [...this.#local.entries()].map(([slot]) => ({ slot, active: true }));
    try {
      this.#runtime.replace_desired({
        connected: this.#desired.connected,
        publications,
        video: this.#desired.video.map((video) => ({
          slot: video.slot,
          trackId: video.publicationId,
          height: video.height,
          minHeight: video.minHeight ?? 0,
          minFps: video.minFps ?? 0,
          priority: video.priority ?? 0,
        })),
        audio: this.#desired.audio,
        playoutDelay: this.#desired.playoutDelay,
        topics,
      });
    } catch (error) {
      throw commandError("desired state was rejected", error);
    }
  }

  #acceptSnapshot(snapshot: RawSnapshot): void {
    if (this.#closed) return;
    this.#raw = snapshot;
    this.#publish();
    if (
      this.#closing &&
      (snapshot.connection === "disconnected" || snapshot.connection === "terminal-failure")
    ) {
      this.#finishClose();
    }
  }

  #acceptEvent(event: RawEvent): void {
    if (this.#closed || typeof event.type !== "string") return;
    if (event.type === "topic-message") {
      this.#deliverTopicMessage(event);
    } else if (event.type === "topic-resynchronized") {
      this.#deliverTopicResynchronization(event);
    } else if (event.type === "topic-channel-failed" && typeof event.message === "string") {
      this.#failure = Object.freeze({ class: "browser", message: event.message });
      this.#localVersion += 1;
      this.#publish();
    }
  }

  #deliverTopicMessage(event: RawEvent): void {
    if (
      (event.mode !== "latest" && event.mode !== "ordered") ||
      typeof event.topic !== "string" ||
      !(event.payload instanceof Uint8Array)
    ) {
      return;
    }
    const topic = this.#topics.get(topicKey(event.topic, event.mode));
    if (!topic?.options.subscribe) return;
    const publisherId = typeof event.publisherId === "string" ? event.publisherId : undefined;
    if (topic.options.publisherId && topic.options.publisherId !== publisherId) return;
    const delivery: TopicDelivery =
      event.mode === "latest"
        ? Object.freeze({
            type: "message",
            mode: "latest",
            publisherId,
            payload: event.payload.slice(),
          })
        : Object.freeze({
            type: "message",
            mode: "ordered",
            publisherId: publisherId ?? "",
            streamId: toBigInt(event.streamId),
            sequence: toBigInt(event.sequence),
            payload: event.payload.slice(),
          });
    if (delivery.mode === "ordered" && delivery.publisherId === "") return;
    topic.deliver(delivery);
  }

  #deliverTopicResynchronization(event: RawEvent): void {
    if (
      typeof event.topic !== "string" ||
      typeof event.publisherId !== "string" ||
      event.streamId === undefined ||
      event.nextSequence === undefined
    ) {
      return;
    }
    const topic = this.#topics.get(topicKey(event.topic, "ordered"));
    if (!topic?.options.subscribe) return;
    topic.deliver(
      Object.freeze({
        type: "resynchronized",
        publisherId: event.publisherId,
        streamId: toBigInt(event.streamId),
        nextSequence: toBigInt(event.nextSequence),
      }),
    );
  }

  #publish(): void {
    const key = this.#snapshotKey();
    if (key === this.#projectionKey) return;
    this.#projectionKey = key;
    this.#snapshot = this.#buildSnapshot();
    for (const listener of [...this.#listeners]) listener();
  }

  #snapshotKey(): string {
    const tracks = [...this.#raw.video, ...this.#raw.audio]
      .map((binding) => `${binding.mid}:${this.#runtime.remote_track(binding.mid)?.id ?? ""}`)
      .join("|");
    return `${this.#raw.version}:${this.#localVersion}:${this.#closed}:${tracks}`;
  }

  #buildSnapshot(): AgentSnapshot {
    const publications: Publication[] = this.#closed
      ? []
      : this.#raw.publications.map((publication) => Object.freeze({ ...publication }));
    const videoBindings = new Map(this.#raw.video.map((binding) => [binding.trackId, binding]));
    const audioBindings = new Map(this.#raw.audio.map((binding) => [binding.trackId, binding]));
    const active = new Set(publications.map((publication) => publication.id));
    const remoteVideo: RemoteVideo[] = [];
    const remoteAudio: RemoteAudio[] = [];
    for (const publication of publications) {
      const binding =
        publication.kind === "video"
          ? videoBindings.get(publication.id)
          : audioBindings.get(publication.id);
      const resource = this.#remoteResource(publication.id);
      const track = binding ? this.#runtime.remote_track(binding.mid) : undefined;
      replaceResourceTrack(resource, track);
      if (publication.kind === "video") {
        remoteVideo.push(
          Object.freeze({
            publicationId: publication.id,
            participantId: publication.participantId,
            stream: resource.stream,
            track,
            bound: binding !== undefined,
            paused: (binding as RawVideoBinding | undefined)?.paused ?? true,
          }),
        );
      } else {
        remoteAudio.push(
          Object.freeze({
            publicationId: publication.id,
            participantId: publication.participantId,
            stream: resource.stream,
            track,
            bound: binding !== undefined,
            levelDbov: (binding as RawAudioBinding | undefined)?.levelDbov,
          }),
        );
      }
    }
    for (const [publicationId, resource] of this.#remote) {
      if (active.has(publicationId)) continue;
      replaceResourceTrack(resource, undefined);
      this.#remote.delete(publicationId);
    }
    const localPublications: LocalPublication[] = [
      ...this.#config.topology.localVideo.map((slot) => this.#localPublication(slot, "video")),
      ...this.#config.topology.localAudio.map((slot) => this.#localPublication(slot, "audio")),
    ];
    return Object.freeze({
      version: this.#raw.version,
      desiredRevision: this.#raw.desiredRevision,
      connection: this.#closed ? "closed" : this.#raw.connection,
      generation: this.#raw.generation,
      participantId: this.#raw.participantId,
      participants: Object.freeze(this.#raw.participants.map((participant) => Object.freeze({ ...participant }))),
      publications: Object.freeze(publications),
      localPublications: Object.freeze(localPublications),
      remoteVideo: Object.freeze(remoteVideo),
      remoteAudio: Object.freeze(remoteAudio),
      topics: freezeTopicState(this.#raw.topics ?? EMPTY_TOPICS),
      failure: this.#raw.failure ? Object.freeze({ ...this.#raw.failure }) : this.#failure,
    });
  }

  #localPublication(slot: string, kind: MediaKind): LocalPublication {
    const local = this.#local.get(slot);
    return Object.freeze({
      slot,
      kind,
      track: local?.track,
      muted: local?.muted ?? false,
      policy: local?.policy ?? (kind === "video" ? "motion" : "speech"),
    });
  }

  #remoteResource(publicationId: string): RemoteResource {
    let resource = this.#remote.get(publicationId);
    if (!resource) {
      resource = { stream: new MediaStream() };
      this.#remote.set(publicationId, resource);
    }
    return resource;
  }

  #finishClose(): void {
    if (this.#closed) return;
    this.#runtime.abort();
    this.#closed = true;
    this.#closing = false;
    this.#local.clear();
    for (const topic of this.#topics.values()) topic.detach();
    this.#topics.clear();
    for (const resource of this.#remote.values()) replaceResourceTrack(resource, undefined);
    this.#remote.clear();
    this.#localVersion += 1;
    this.#publish();
    this.#listeners.clear();
    this.#resolveClose?.();
    this.#resolveClose = undefined;
    queueMicrotask(() => this.#runtime.free());
  }
}

interface RawStatistics {
  connection: string;
  report: RTCStatsReport;
  senders: Array<{
    slot: string;
    kind: MediaKind;
    trackId?: string;
    parameters: RTCRtpSendParameters;
  }>;
}

function projectStatistics(raw: RawStatistics): AgentStatistics {
  let timestamp = 0;
  let bytesSent = 0;
  let bytesReceived = 0;
  let packetsLost = 0;
  raw.report.forEach((entry) => {
    const stats = entry as unknown as Record<string, unknown>;
    timestamp = Math.max(timestamp, numberValue(stats.timestamp));
    bytesSent += numberValue(stats.bytesSent);
    bytesReceived += numberValue(stats.bytesReceived);
    packetsLost += numberValue(stats.packetsLost);
  });
  const senders: SenderStatistics[] = raw.senders.map((sender) => {
    const encodings: SenderEncodingStatistics[] = sender.parameters.encodings.map((encoding) => {
      const extended = encoding as RTCRtpEncodingParameters & {
        scalabilityMode?: string;
        dtx?: "enabled" | "disabled";
      };
      return Object.freeze({
        rid: encoding.rid,
        active: encoding.active ?? false,
        maxBitrate: encoding.maxBitrate,
        maxFramerate: encoding.maxFramerate,
        scaleResolutionDownBy: encoding.scaleResolutionDownBy,
        scalabilityMode: extended.scalabilityMode,
        dtx: extended.dtx,
      });
    });
    return Object.freeze({
      slot: sender.slot,
      kind: sender.kind,
      trackId: sender.trackId,
      encodings: Object.freeze(encodings),
    });
  });
  return Object.freeze({
    timestamp,
    connection: raw.connection,
    bytesSent,
    bytesReceived,
    packetsLost,
    senders: Object.freeze(senders),
  });
}

function normalizeConfig(config: AgentConfig): NormalizedConfig {
  if (!config || typeof config !== "object") {
    throw new PulseBeamError("invalid-config", "agent configuration is required");
  }
  const endpoint = config.endpoint?.replace(/\/+$/, "");
  let url: URL;
  try {
    url = new URL(endpoint);
  } catch (error) {
    throw new PulseBeamError("invalid-config", "endpoint must be an absolute HTTP(S) URL", error);
  }
  if (!['http:', 'https:'].includes(url.protocol) || url.search || url.hash) {
    throw new PulseBeamError("invalid-config", "endpoint must be an absolute HTTP(S) URL");
  }
  if (!validIdentifier(config.roomId, false, 256)) {
    throw new PulseBeamError("invalid-config", "roomId is invalid");
  }
  const localVideo = normalizeSlots(config.topology?.localVideo ?? [], "localVideo", 2);
  const localAudio = normalizeSlots(config.topology?.localAudio ?? [], "localAudio", 2);
  const allSlots = [...localVideo, ...localAudio];
  if (new Set(allSlots).size !== allSlots.length) {
    throw new PulseBeamError("invalid-config", "local publication slot names must be unique");
  }
  const remoteVideo = boundedInteger(config.topology?.remoteVideo ?? 0, "remoteVideo", 7);
  const remoteAudio = boundedInteger(config.topology?.remoteAudio ?? 0, "remoteAudio", 3);
  const requestHeaders = normalizeHeaders(config.requestHeaders ?? {});
  return Object.freeze({
    endpoint,
    roomId: config.roomId,
    requestHeaders,
    topology: Object.freeze({ localVideo, localAudio, remoteVideo, remoteAudio }),
  });
}

function normalizeSlots(slots: readonly string[], name: string, maximum: number): readonly string[] {
  if (!Array.isArray(slots) || slots.length > maximum || slots.some((slot) => !validIdentifier(slot))) {
    throw new PulseBeamError("invalid-config", `${name} contains invalid slot names`);
  }
  if (new Set(slots).size !== slots.length) {
    throw new PulseBeamError("invalid-config", `${name} contains duplicate slot names`);
  }
  return Object.freeze([...slots]);
}

function normalizeHeaders(headers: Readonly<Record<string, string>>): Readonly<Record<string, string>> {
  const protocolOwned = new Set(["content-type", "content-length", "host", "if-match"]);
  const normalized: Record<string, string> = {};
  for (const [name, value] of Object.entries(headers)) {
    if (
      !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(name) ||
      protocolOwned.has(name.toLowerCase()) ||
      typeof value !== "string" ||
      /[^\t\x20-\x7e]/.test(value)
    ) {
      throw new PulseBeamError("invalid-config", `request header is invalid or reserved: ${name}`);
    }
    normalized[name] = value;
  }
  return Object.freeze(normalized);
}

function normalizeVideoDemand(demand: readonly VideoDemand[], capacity: number): readonly VideoDemand[] {
  if (!Array.isArray(demand)) {
    throw new PulseBeamError("invalid-command", "video demand must be an array");
  }
  const slots = new Set<number>();
  const publications = new Set<string>();
  const normalized = demand.map((item) => {
    if (
      !Number.isInteger(item.slot) ||
      item.slot < 0 ||
      item.slot >= capacity ||
      slots.has(item.slot) ||
      !validIdentifier(item.publicationId, true, 256) ||
      publications.has(item.publicationId)
    ) {
      throw new PulseBeamError("invalid-command", "video demand has invalid or duplicate slots");
    }
    const height = nonNegativeInteger(item.height, "height");
    const minHeight = nonNegativeInteger(item.minHeight ?? 0, "minHeight");
    if (minHeight > height || (height === 0 && minHeight !== 0)) {
      throw new PulseBeamError("invalid-command", "video minHeight cannot exceed height");
    }
    slots.add(item.slot);
    publications.add(item.publicationId);
    return Object.freeze({
      slot: item.slot,
      publicationId: item.publicationId,
      height,
      minHeight,
      minFps: nonNegativeInteger(item.minFps ?? 0, "minFps"),
      priority: nonNegativeInteger(item.priority ?? 0, "priority"),
    });
  });
  return Object.freeze(normalized);
}

function normalizePlayoutDelay(delay: PlayoutDelay): PlayoutDelay {
  if (delay.mode === "adaptive") return Object.freeze({ mode: "adaptive" });
  const minMs = nonNegativeInteger(delay.minMs, "minMs");
  const maxMs = nonNegativeInteger(delay.maxMs, "maxMs");
  if (minMs > maxMs || maxMs > 40_950) {
    throw new PulseBeamError("invalid-command", "playout delay bounds are invalid");
  }
  return Object.freeze({ mode: "fixed", minMs, maxMs });
}

function normalizeTopicOptions(options: TopicOptions): Readonly<TopicOptions> {
  if (!/^[A-Za-z0-9_-]+$/.test(options.name) || options.name.length > 64) {
    throw new PulseBeamError("invalid-command", "topic name is invalid");
  }
  if (options.mode !== "latest" && options.mode !== "ordered") {
    throw new PulseBeamError("invalid-command", "topic mode must be latest or ordered");
  }
  const publish = options.publish ?? false;
  const subscribe = options.subscribe ?? false;
  if (!publish && !subscribe) {
    throw new PulseBeamError("invalid-command", "topic must publish, subscribe, or both");
  }
  if (options.mode === "ordered" && options.publisherId) {
    throw new PulseBeamError("invalid-command", "ordered topics cannot scope one publisher");
  }
  if (options.publisherId && !validIdentifier(options.publisherId, false, 64)) {
    throw new PulseBeamError("invalid-command", "topic publisherId is invalid");
  }
  return Object.freeze({
    name: options.name,
    mode: options.mode,
    publish,
    subscribe,
    publisherId: options.publisherId,
  });
}

function resolvePolicy(
  kind: MediaKind,
  policy?: VideoPolicy | AudioPolicy,
): VideoPolicy | AudioPolicy {
  const resolved = policy ?? (kind === "video" ? "motion" : "speech");
  if (
    (kind === "video" && resolved !== "motion" && resolved !== "detail") ||
    (kind === "audio" && resolved !== "speech" && resolved !== "music")
  ) {
    throw new PulseBeamError("invalid-command", `${resolved} is not a ${kind} policy`);
  }
  return resolved;
}

function freezeTopicState(topics: RawTopicState): TopicState {
  return Object.freeze({
    publishers: Object.freeze(topics.publishers.map((publisher) => Object.freeze({ ...publisher }))),
    subscribers: Object.freeze(topics.subscribers.map((subscriber) => Object.freeze({ ...subscriber }))),
    acceptedSends: topics.acceptedSends,
    droppedSends: topics.droppedSends,
    deliveredMessages: topics.deliveredMessages,
    resynchronizations: topics.resynchronizations,
    channelFailures: topics.channelFailures,
  });
}

function replaceResourceTrack(resource: RemoteResource, track: MediaStreamTrack | undefined): void {
  if (resource.track === track) return;
  for (const current of resource.stream.getTracks()) resource.stream.removeTrack(current);
  if (track) resource.stream.addTrack(track);
  resource.track = track;
}

function topicKey(name: string, mode: TopicMode): string {
  return `${mode}\0${name}`;
}

function validIdentifier(value: unknown, allowSlash = false, maximum = 64): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= maximum &&
    !/[\u0000-\u001f\u007f]/.test(value) &&
    (allowSlash || !value.includes("/"))
  );
}

function boundedInteger(value: number, name: string, maximum: number): number {
  if (!Number.isInteger(value) || value < 0 || value > maximum) {
    throw new PulseBeamError("invalid-config", `${name} must be between 0 and ${maximum}`);
  }
  return value;
}

function nonNegativeInteger(value: number, name: string): number {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) {
    throw new PulseBeamError("invalid-command", `${name} must be a non-negative integer`);
  }
  return value;
}

function sameJson(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function toBigInt(value: unknown): bigint {
  if (typeof value === "bigint") return value;
  if (typeof value === "number" && Number.isSafeInteger(value)) return BigInt(value);
  if (typeof value === "string") return BigInt(value);
  throw new PulseBeamError("runtime", "browser delivered an invalid integer");
}

function numberValue(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function commandError(message: string, error: unknown): PulseBeamError {
  return new PulseBeamError("runtime", `${message}: ${errorMessage(error)}`, error);
}
