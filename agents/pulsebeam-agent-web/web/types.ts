export type MediaKind = "audio" | "video";
export type TopicMode = "latest" | "ordered";
export type LogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export interface AgentLogging {
  /** Maximum verbosity emitted by this Agent. Defaults to `warn`. */
  readonly level?: LogLevel;
}

export interface MediaTopology {
  readonly localVideo?: readonly string[];
  readonly localAudio?: readonly string[];
  readonly remoteVideo?: number;
  readonly remoteAudio?: number;
}

export interface AgentConfig {
  /** Absolute HTTP(S) server endpoint. Core appends `/api/v1`. */
  readonly endpoint: string;
  readonly roomId: string;
  readonly requestHeaders?: Readonly<Record<string, string>>;
  readonly topology: MediaTopology;
  readonly logging?: AgentLogging;
}

export interface PublicationIntent {
  readonly slot: string;
  readonly active: boolean;
}

export interface VideoDemand {
  readonly slot: number;
  readonly trackId: string;
  readonly height: number;
  readonly minHeight: number;
  readonly minFps: number;
  readonly priority: number;
}

export interface AudioDemand {
  readonly pinned?: readonly string[];
  readonly automatic?: boolean;
}

export interface FixedPlayoutDelay {
  readonly mode: "fixed";
  readonly minMs: number;
  readonly maxMs: number;
}

export interface TopicRegistration {
  readonly name: string;
  readonly mode: TopicMode;
  readonly publish?: boolean;
  readonly subscribe?: boolean;
  readonly publisherId?: string;
}

/**
 * Complete desired state. Omitted collections are empty and retract their
 * previous desired values. Once fixed, playout delay stays fixed for the
 * lifetime of an agent; create a new agent to return to adaptive mode.
 */
export interface AgentState {
  readonly connected: boolean;
  readonly publications?: readonly PublicationIntent[];
  readonly video?: readonly VideoDemand[];
  readonly audio?: AudioDemand;
  readonly playoutDelay?: FixedPlayoutDelay;
  readonly topics?: readonly TopicRegistration[];
}

export type ConnectionState =
  | "disconnected"
  | "initializing"
  | "creating-offer"
  | "joining"
  | "applying-answer"
  | "waiting-for-transport"
  | "waiting-for-signaling"
  | "connected"
  | "reconnecting"
  | `retry-waiting:${number}`
  | "closing"
  | "terminal-failure";

export type FailureClass =
  | "initialization"
  | "invalid-configuration"
  | "authorization"
  | "protocol"
  | "transient"
  | "resource-expired"
  | "retry-exhausted"
  | "validation"
  | "runtime";

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

export interface VideoBinding {
  readonly trackId: string;
  readonly mid: string;
  readonly paused: boolean;
}

export interface AudioBinding {
  readonly trackId: string;
  readonly mid: string;
  readonly levelDbov: number | null;
}

interface RemoteMediaBase {
  readonly publicationId: string;
  readonly participantId: string;
  readonly mid: string;
  readonly media: MediaStreamTrack;
}

export interface RemoteVideoTrack extends RemoteMediaBase {
  readonly kind: "video";
  readonly paused: boolean;
}

export interface RemoteAudioTrack extends RemoteMediaBase {
  readonly kind: "audio";
  readonly levelDbov: number | null;
}

export type RemoteTrack = RemoteVideoTrack | RemoteAudioTrack;

export interface TopicPublisherStatus {
  readonly name: string;
  readonly mode: TopicMode;
  readonly connected: boolean;
  readonly streamId: number | null;
  readonly nextSequence: number | null;
  readonly queued: number;
  readonly sendPending: boolean;
}

export interface TopicSubscriberStatus {
  readonly name: string;
  readonly mode: TopicMode;
  readonly publisherId: string | null;
  readonly connected: boolean;
  readonly publishers: number;
  readonly buffered: number;
}

export interface TopicSnapshot {
  readonly publishers: readonly TopicPublisherStatus[];
  readonly subscribers: readonly TopicSubscriberStatus[];
  readonly acceptedSends: number;
  readonly droppedSends: number;
  readonly deliveredMessages: number;
  readonly resynchronizations: number;
  readonly channelFailures: number;
}

export interface AgentSnapshot {
  readonly version: number;
  readonly desiredRevision: number;
  readonly connection: ConnectionState;
  readonly generation: number | null;
  readonly participantId: string | null;
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly video: readonly VideoBinding[];
  readonly audio: readonly AudioBinding[];
  /** Available remote tracks, keyed by publication ID. */
  readonly tracks: Readonly<Record<string, RemoteTrack>>;
  readonly topics: TopicSnapshot;
  readonly failure: AgentFailure | null;
}

export interface SenderEncoding {
  readonly rid?: string;
  readonly active: boolean;
  readonly scaleResolutionDownBy?: number;
  readonly maxBitrate?: number;
  readonly maxFramerate?: number;
  readonly scalabilityMode?: "L1T1" | "L1T2" | "L1T3";
  readonly dtx?: "enabled" | "disabled";
}

interface SenderConfigBase {
  readonly degradationPreference?:
    | "maintain-framerate"
    | "maintain-resolution"
    | "balanced";
}

export interface VideoSenderConfig extends SenderConfigBase {
  readonly contentHint: "motion" | "detail" | "text";
  /** Omit or pass an empty tuple to use the runtime's three-layer defaults. */
  readonly encodings?:
    | readonly []
    | readonly [SenderEncoding, SenderEncoding, SenderEncoding];
}

export interface AudioSenderConfig extends SenderConfigBase {
  readonly contentHint: "speech" | "music";
  /** Omit or pass an empty tuple to use the runtime's single-layer default. */
  readonly encodings?: readonly [] | readonly [SenderEncoding];
}

export type SenderConfig = VideoSenderConfig | AudioSenderConfig;

export type TopicDropReason =
  | "invalid-payload"
  | "not-registered"
  | "channel-unavailable"
  | "queue-full"
  | "superseded"
  | "host-rejected"
  | "channel-closed"
  | "transport-replaced"
  | "sequence-exhausted";

export type AgentEvent =
  | {
      readonly type: "topic-message";
      readonly mode: "latest";
      readonly topic: string;
      readonly publisherId: string;
      readonly payload: Uint8Array;
    }
  | {
      readonly type: "topic-message";
      readonly mode: "ordered";
      readonly topic: string;
      readonly publisherId: string;
      readonly streamId: number;
      readonly sequence: number;
      readonly payload: Uint8Array;
    }
  | {
      readonly type: "topic-resynchronized";
      readonly topic: string;
      readonly publisherId: string;
      readonly streamId: number;
      readonly nextSequence: number;
    }
  | {
      readonly type: "topic-send-admitted";
      readonly topic: string;
      readonly mode: TopicMode;
      readonly operation: number;
      readonly streamId: number | null;
      readonly sequence: number | null;
    }
  | {
      readonly type: "topic-send-dropped";
      readonly topic: string;
      readonly mode: TopicMode;
      readonly reason: TopicDropReason;
    }
  | {
      readonly type: "topic-channel-failed";
      readonly direction: "publish" | "subscribe";
      readonly topic: string;
      readonly mode: TopicMode;
      readonly message: string;
    }
  | ({ readonly type: "failure" } & AgentFailure)
  | { readonly type: "state-change" };

export interface Agent {
  setState(state: AgentState): void;
  replaceLocalTrack(
    slot: string,
    track: MediaStreamTrack | null,
    config: SenderConfig,
  ): Promise<void>;
  setLocalMuted(slot: string, muted: boolean): Promise<void>;
  reconnect(): void;
  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): void;
  readonly getSnapshot: () => AgentSnapshot;
  readonly subscribe: (listener: () => void) => () => void;
  readonly subscribeEvents: (
    listener: (event: AgentEvent) => void,
  ) => () => void;
  close(): void;
}

export type PlaybackFailure = Readonly<{
  message: string;
  cause?: unknown;
}>;

export type RemoteMediaAttachmentOptions = Readonly<{
  publicationIds: readonly string[];
  onPlaybackBlocked?: (
    failure: PlaybackFailure,
    retry: () => Promise<void>,
  ) => void;
}>;

export interface RemoteMediaAttachment {
  setPublicationIds(publicationIds: readonly string[]): void;
  retryPlayback(): Promise<void>;
  close(): void;
}
