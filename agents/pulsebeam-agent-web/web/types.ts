import type {
  CapturedAudioTrack,
  CapturedVideoTrack,
} from "./capture-source.js";

import type {
  AudioTrackSpec,
  DataTrackSpec,
  Delivery,
  VideoTrackSpec,
} from "./track.js";
import type { LocalDataTrack, RemoteDataTrack } from "./data.js";

export type MediaKind = "audio" | "video";

export interface LocalVideoTrack {
  readonly kind: "video";
  readonly label: string;
  readonly source: CapturedVideoTrack | null;
  readonly enabled: boolean;
  setEnabled(enabled: boolean): void;
  setSource(source: CapturedVideoTrack | null): void;
  subscribe(listener: () => void): () => void;
}

export interface LocalAudioTrack {
  readonly kind: "audio";
  readonly label: string;
  readonly source: CapturedAudioTrack | null;
  readonly enabled: boolean;
  setEnabled(enabled: boolean): void;
  setSource(source: CapturedAudioTrack | null): void;
  subscribe(listener: () => void): () => void;
}
export type TopicMode = "latest" | "lossy" | "ordered";
export type LogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export interface AgentLogging {
  /** Maximum verbosity emitted by this Agent. Defaults to `warn`. */
  readonly level?: LogLevel;
}

export interface MediaTopology {
  readonly localVideos?: number;
  readonly localAudios?: number;
  readonly remoteVideos?: number;
  readonly remoteAudios?: number;
}

export interface AgentConfig {
  /** Absolute HTTP(S) server endpoint. Core appends `/api/v1`. */
  readonly endpoint: string;
  /** Opaque bearer token issued by the application backend. */
  readonly token: string;
  /** Optional fixed media capacity. Omitted fields use SDK defaults. */
  readonly topology?: MediaTopology;
  readonly logging?: AgentLogging;
}

export interface PublicationIntent {
  readonly slot: string;
  readonly label: string;
  readonly active: boolean;
}

export interface TrackSelector {
  readonly participantId: string;
  readonly label: string;
}

export type VideoDemand = {
  readonly slot: number;
  readonly height: number;
  readonly minHeight: number;
  readonly minFps: number;
  readonly priority: number;
  readonly playoutDelay?: FixedPlayoutDelay;
} & (
  | { readonly trackId: string; readonly selector?: never }
  | { readonly trackId?: never; readonly selector: TrackSelector }
);

export interface AudioDemand {
  readonly pinned?: readonly string[];
  readonly selected?: readonly TrackSelector[];
  readonly automatic?: boolean;
  readonly playoutDelays?: readonly {
    readonly trackId: string;
    readonly playoutDelay: FixedPlayoutDelay;
  }[];
  readonly selectorDelays?: readonly {
    readonly selector: TrackSelector;
    readonly playoutDelay: FixedPlayoutDelay;
  }[];
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
}

/**
 * Complete desired state. Omitted collections are empty and retract their
 * previous desired values. Playout policy belongs to each explicitly
 * requested track, not to the connection or automatic audio.
 */
export interface AgentState {
  readonly connected: boolean;
  readonly publications?: readonly PublicationIntent[];
  readonly video?: readonly VideoDemand[];
  readonly audio?: AudioDemand;
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

export interface AgentError {
  readonly class: FailureClass;
  readonly message: string;
}

/** @deprecated Use AgentError. */
export type AgentFailure = AgentError;

export interface Participant {
  readonly participantId: string;
}

export interface Publication {
  readonly id: string;
  readonly participantId: string;
  readonly kind: MediaKind;
  readonly label: string;
}

interface RemoteMediaBase {
  readonly publicationId: string;
  readonly participantId: string;
  readonly media: MediaStreamTrack;
}

export interface MappedRemoteVideoTrack extends RemoteMediaBase {
  readonly kind: "video";
}

export interface MappedRemoteAudioTrack extends RemoteMediaBase {
  readonly kind: "audio";
}

export type RemoteTrack = MappedRemoteVideoTrack | MappedRemoteAudioTrack;

export interface ReceiveOptions {
  readonly minHeight?: number;
  readonly minFps?: number;
  readonly priority?: number;
  readonly playoutDelay?: Readonly<{ minMs: number; maxMs: number }>;
}

export interface RemoteVideoTrack {
  readonly kind: "video";
  readonly id: string | null;
  /** External application participant identity, not a canonical runtime ID. */
  readonly participantId: string;
  readonly label: string;
  readonly active: boolean;
  subscribe(listener: () => void): () => void;
}

export interface RemoteAudioTrack {
  readonly kind: "audio";
  readonly id: string | null;
  readonly active: boolean;
  readonly participantId: string;
  readonly label: string;
  subscribe(listener: () => void): () => void;
}

export interface LocalMedia {
  video(label: string): LocalVideoTrack;
  audio(label: string): LocalAudioTrack;
}

export type RemotePublishedTrack =
  | RemoteVideoTrack
  | RemoteAudioTrack
  | RemoteDataTrack;

export interface RemoteParticipant {
  readonly participantId: string;
  readonly active: boolean;
  readonly tracks: readonly RemotePublishedTrack[];
  track(spec: VideoTrackSpec): RemoteVideoTrack;
  track(spec: AudioTrackSpec): RemoteAudioTrack;
  track<D extends Delivery>(spec: DataTrackSpec<D>): RemoteDataTrack<D>;
}

export interface RemoteMedia {
  readonly participants: readonly RemoteParticipant[];
  participant(participantId: string): RemoteParticipant;
}

export type TopicEvent<T> =
  | {
      readonly type: "message";
      readonly dataTrackId: string;
      readonly sequence: bigint;
      readonly participantId: string;
      readonly topic: string;
      readonly value: T;
    }
  | {
      readonly type: "gap";
      readonly dataTrackId: string;
      readonly participantId: string;
      readonly topic: string;
      readonly expectedSequence: bigint;
      readonly availableSequence: bigint;
    };
export type TopicGap = Extract<TopicEvent<never>, { readonly type: "gap" }>;
export type TopicConsumerError =
  | {
      readonly code: "decode-failed";
      readonly topic: string;
      readonly cause: unknown;
    }
  | {
      readonly code: "channel-failed";
      readonly event: Extract<
        AgentEvent,
        { readonly type: "topic-channel-failed" }
      >;
    };
export interface TopicCallbacks<T> {
  readonly onEvent: (event: TopicEvent<T>) => void;
  readonly onError?: (error: TopicConsumerError) => void;
}
export interface Topic<T> {
  /** Local transmission acceptance only, not remote delivery or persistence. */
  publish(value: T): Promise<void>;
  subscribe(options?: {
    readonly signal?: AbortSignal;
  }): AsyncIterable<TopicEvent<T>>;
}

export interface TopicPublisherStatus {
  readonly name: string;
  readonly mode: TopicMode;
  readonly connected: boolean;
  readonly dataTrackId: string | null;
  readonly nextSequence: bigint;
  readonly acceptedHistory: number;
  readonly queued: number;
  readonly sendPending: boolean;
}

export interface TopicSubscriberStatus {
  readonly name: string;
  readonly mode: TopicMode;
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
  readonly channelFailures: number;
}

export type CatalogTrack = Readonly<{
  id: string;
  participantId: string;
  label: string;
}> &
  (
    | Readonly<{ kind: MediaKind }>
    | Readonly<{ kind: "data"; delivery: TopicMode }>
  );

export interface CatalogSnapshot {
  readonly revision: number;
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly tracks: readonly CatalogTrack[];
}

export interface TrackMapping {
  readonly receiverIndex: number;
  readonly publicationId: string;
}

export interface MappingSnapshot {
  readonly acceptedIntentRevision: number;
  readonly video: readonly TrackMapping[];
  readonly audio: readonly TrackMapping[];
}

export interface AgentSnapshot {
  readonly version: number;
  readonly desiredRevision: number;
  readonly connection: ConnectionState;
  readonly generation: number | null;
  readonly participantId: string | null;
  /** Authoritative application room identity, retained during reconnection. */
  readonly roomId: string | null;
  /** Current authorization deadline, in Unix seconds. */
  readonly authorizationExpiresAt: number | null;
  readonly catalog: CatalogSnapshot;
  readonly mapping: MappingSnapshot;
  /** Available remote tracks, keyed by publication ID. */
  readonly tracks: Readonly<Record<string, RemoteTrack>>;
  readonly topics: TopicSnapshot;
  readonly failure: AgentError | null;
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
      readonly mode: TopicMode;
      readonly topic: string;
      readonly participantId: string;
      readonly dataTrackId: string;
      readonly sequence: bigint;
      readonly payload: Uint8Array;
    }
  | {
      readonly type: "topic-recovery-gap";
      readonly topic: string;
      readonly participantId: string;
      readonly dataTrackId: string;
      readonly expectedSequence: bigint;
      readonly availableSequence: bigint;
    }
  | {
      readonly type: "topic-send-admitted";
      readonly topic: string;
      readonly mode: TopicMode;
      readonly operation: bigint;
      readonly dataTrackId: string;
      readonly sequence: bigint;
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
  | ({ readonly type: "failure" } & AgentError)
  | { readonly type: "state-change" };

export interface Agent {
  /** Current observed connection state. Reactive when returned by React useAgent(). */
  readonly connection: ConnectionState;
  /** Current Agent error, if any. Reactive when returned by React useAgent(). */
  readonly error: AgentError | null;
  readonly participantId: string | null;
  readonly roomId: string | null;
  readonly authorizationExpiresAt: number | null;
  readonly remote: RemoteMedia;
  track(spec: VideoTrackSpec): LocalVideoTrack;
  track(spec: AudioTrackSpec): LocalAudioTrack;
  track<D extends Delivery>(spec: DataTrackSpec<D>): LocalDataTrack<D>;
  connect(): void;
  disconnect(): void;
  renewAuthorization(token: string): void;
  setLogging(level: LogLevel): void;
  readonly getSnapshot: () => AgentSnapshot;
  readonly subscribe: (listener: () => void) => () => void;
  close(): void;
}

export type PlaybackFailure = Readonly<{
  message: string;
  cause?: unknown;
  retry(): Promise<void>;
}>;

export type RemoteMediaAttachmentOptions = Readonly<{
  publicationIds: readonly string[];
  autoPlay?: boolean;
  onPlaybackBlocked?: (
    failure: PlaybackFailure,
    retry: () => Promise<void>,
  ) => void;
}>;

export interface RemoteMediaAttachment {
  setPublicationIds(publicationIds: readonly string[]): void;
  setAutoPlay(autoPlay: boolean): void;
  retryPlayback(): Promise<void>;
  close(): void;
}
