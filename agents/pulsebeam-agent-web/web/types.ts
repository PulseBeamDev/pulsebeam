export type MediaKind = "audio" | "video";
export type TopicMode = "latest" | "ordered";
export type VideoPolicy = "motion" | "detail";
export type AudioPolicy = "speech" | "music";
export type LogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export interface AgentTopology {
  localVideo?: readonly string[];
  localAudio?: readonly string[];
  remoteVideo?: number;
  remoteAudio?: number;
}

export interface AgentConfig {
  endpoint: string;
  roomId: string;
  requestHeaders?: Readonly<Record<string, string>>;
  topology: AgentTopology;
}

export interface VideoDemand {
  slot: number;
  publicationId: string;
  height: number;
  minHeight?: number;
  minFps?: number;
  priority?: number;
}

export interface AudioDemand {
  pinned?: readonly string[];
  automatic?: boolean;
}

export type PlayoutDelay =
  | Readonly<{ mode: "adaptive" }>
  | Readonly<{ mode: "fixed"; minMs: number; maxMs: number }>;

export interface TopicOptions {
  name: string;
  mode: TopicMode;
  publish?: boolean;
  subscribe?: boolean;
  publisherId?: string;
}

export type TopicDelivery =
  | Readonly<{
      type: "message";
      mode: "latest";
      publisherId?: string;
      payload: Uint8Array;
    }>
  | Readonly<{
      type: "message";
      mode: "ordered";
      publisherId: string;
      streamId: bigint;
      sequence: bigint;
      payload: Uint8Array;
    }>
  | Readonly<{
      type: "resynchronized";
      publisherId: string;
      streamId: bigint;
      nextSequence: bigint;
    }>;

export interface Topic extends AsyncIterable<TopicDelivery> {
  readonly name: string;
  readonly mode: TopicMode;
  readonly closed: boolean;
  send(payload: Uint8Array): void;
  subscribe(listener: (delivery: TopicDelivery) => void): () => void;
  close(): void;
}

export type ConnectionState =
  | "disconnected"
  | "creating-offer"
  | "joining"
  | "applying-answer"
  | "waiting-for-transport"
  | "waiting-for-signaling"
  | "connected"
  | "reconnecting"
  | `retry-waiting:${number}`
  | "closing"
  | "terminal-failure"
  | "closed";

export type FailureClass =
  | "invalid-configuration"
  | "authorization"
  | "protocol"
  | "transient"
  | "resource-expired"
  | "retry-exhausted"
  | "browser";

export interface AgentFailure {
  readonly class: FailureClass;
  readonly message: string;
}

export interface Participant {
  readonly id: string;
}

export interface Publication {
  readonly id: string;
  readonly participantId: string;
  readonly kind: MediaKind;
}

export interface LocalPublication {
  readonly slot: string;
  readonly kind: MediaKind;
  readonly track?: MediaStreamTrack;
  readonly muted: boolean;
  readonly policy: VideoPolicy | AudioPolicy;
}

export interface RemoteVideo {
  readonly publicationId: string;
  readonly participantId: string;
  readonly stream: MediaStream;
  readonly track?: MediaStreamTrack;
  readonly bound: boolean;
  readonly paused: boolean;
}

export interface RemoteAudio {
  readonly publicationId: string;
  readonly participantId: string;
  readonly stream: MediaStream;
  readonly track?: MediaStreamTrack;
  readonly bound: boolean;
  readonly levelDbov?: number;
}

export interface TopicPublisherState {
  readonly name: string;
  readonly mode: TopicMode;
  readonly connected: boolean;
  readonly streamId?: bigint;
  readonly nextSequence?: bigint;
  readonly queued: number;
  readonly sendPending: boolean;
}

export interface TopicSubscriberState {
  readonly name: string;
  readonly mode: TopicMode;
  readonly publisherId?: string;
  readonly connected: boolean;
  readonly publishers: number;
  readonly buffered: number;
}

export interface TopicState {
  readonly publishers: readonly TopicPublisherState[];
  readonly subscribers: readonly TopicSubscriberState[];
  readonly acceptedSends: bigint;
  readonly droppedSends: bigint;
  readonly deliveredMessages: bigint;
  readonly resynchronizations: bigint;
  readonly channelFailures: bigint;
}

export interface AgentSnapshot {
  readonly version: bigint;
  readonly desiredRevision: bigint;
  readonly connection: ConnectionState;
  readonly generation?: bigint;
  readonly participantId?: string;
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly localPublications: readonly LocalPublication[];
  readonly remoteVideo: readonly RemoteVideo[];
  readonly remoteAudio: readonly RemoteAudio[];
  readonly topics: TopicState;
  readonly failure?: AgentFailure;
}

export interface SenderEncodingStatistics {
  readonly rid?: string;
  readonly active: boolean;
  readonly maxBitrate?: number;
  readonly maxFramerate?: number;
  readonly scaleResolutionDownBy?: number;
  readonly scalabilityMode?: string;
  readonly dtx?: "enabled" | "disabled";
}

export interface SenderStatistics {
  readonly slot: string;
  readonly kind: MediaKind;
  readonly trackId?: string;
  readonly encodings: readonly SenderEncodingStatistics[];
}

export interface AgentStatistics {
  readonly timestamp: number;
  readonly connection: string;
  readonly bytesSent: number;
  readonly bytesReceived: number;
  readonly packetsLost: number;
  readonly senders: readonly SenderStatistics[];
}

export interface Agent {
  getSnapshot(): AgentSnapshot;
  subscribe(listener: () => void): () => void;
  connect(): void;
  reconnect(): void;
  close(): Promise<void>;
  abort(): void;
  setLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    policy?: VideoPolicy | AudioPolicy,
  ): Promise<void>;
  setLocalMuted(slot: string, muted: boolean): Promise<void>;
  setVideoDemand(demand: readonly VideoDemand[]): void;
  setAudioDemand(demand: AudioDemand): void;
  setPlayoutDelay(delay: PlayoutDelay): void;
  openTopic(options: TopicOptions): Topic;
  statistics(): Promise<AgentStatistics>;
}

export type PulseBeamErrorCode =
  | "invalid-config"
  | "invalid-command"
  | "closed"
  | "runtime";

export class PulseBeamError extends Error {
  readonly code: PulseBeamErrorCode;
  override readonly cause?: unknown;

  constructor(code: PulseBeamErrorCode, message: string, cause?: unknown) {
    super(message);
    this.name = "PulseBeamError";
    this.code = code;
    this.cause = cause;
  }
}

export type LogSink = (level: LogLevel, target: string, message: string) => void;
