import type { EventStream, SnapshotStream } from "./observation.js";
import type {
  AgentState as ObservedAgentState,
  ErrorInfo,
  WatchStatus,
  CatalogParticipantInfo as CoreCatalogParticipantInfo,
  CatalogPublicationInfo as CoreCatalogPublicationInfo,
  CatalogPublicationKind as CoreCatalogPublicationKind,
  PublicationState as ObservedPublicationState,
  PipeState as ObservedPipeState,
  VideoSubscriptionState as ObservedVideoSubscriptionState,
  AudioSubscriptionState as ObservedAudioSubscriptionState,
  DataPublicationState as ObservedDataPublicationState,
  DataSubscriptionState as ObservedDataSubscriptionState,
} from "./schema.js";
import type { AudioPublication, VideoPublication } from "./media.js";
import type {
  AudioSelection,
  AudioSubscription,
  VideoSubscription,
} from "./reception.js";

import type { Delivery } from "./track.js";
import type { LocalDataTrack, RemoteDataTrack } from "./data.js";

export type MediaKind = "audio" | "video";

export interface PublicationConfig {
  readonly enabled: boolean;
}

export interface PipeUpdates {
  next(): Promise<Readonly<ObservedPipeState> | undefined>;
  close(): void;
}

export interface PublicationUpdates {
  next(): Promise<Readonly<ObservedPublicationState> | undefined>;
  close(): void;
}

export interface VideoSubscriptionUpdates {
  next(): Promise<Readonly<ObservedVideoSubscriptionState> | undefined>;
  close(): void;
}
export interface AudioSubscriptionUpdates {
  next(): Promise<Readonly<ObservedAudioSubscriptionState> | undefined>;
  close(): void;
}
export interface DataPublicationUpdates {
  next(): Promise<Readonly<ObservedDataPublicationState> | undefined>;
  close(): void;
}
export interface DataSubscriptionUpdates {
  next(): Promise<Readonly<ObservedDataSubscriptionState> | undefined>;
  close(): void;
}

export interface LocalVideoTrack {
  readonly kind: "video";
  readonly trackId: string;
  publish(configuration?: PublicationConfig): VideoPublication;
}

export interface LocalAudioTrack {
  readonly kind: "audio";
  readonly trackId: string;
  publish(configuration?: PublicationConfig): AudioPublication;
}
export type TopicMode = "latest" | "lossy" | "ordered";
export type LogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export interface AgentLogging {
  /** Maximum verbosity emitted by this Agent. Defaults to `warn`. */
  readonly level?: LogLevel;
}

export interface AgentConfig {
  /** Absolute HTTP(S) server endpoint. Core appends `/api/v1`. */
  readonly endpoint: string;
  /** Opaque bearer token issued by the application backend. */
  readonly token: string;
  readonly logging?: AgentLogging;
}

export interface PublicationIntent {
  readonly slot: string;
  readonly kind: MediaKind;
  readonly trackId: string;
  readonly active: boolean;
}

export interface TrackSelector {
  readonly room: string;
  readonly participantId: string;
  readonly trackId: string;
}

export type VideoDemand = {
  readonly slot: number;
  readonly height: number;
  readonly minHeight: number;
  readonly minFps: number;
  readonly priority: number;
  readonly playoutDelay?: FixedPlayoutDelay;
} & (
  | { readonly publicationId: string; readonly selector?: never }
  | { readonly publicationId?: never; readonly selector: TrackSelector }
);

export interface AudioDemand {
  readonly pinned?: readonly string[];
  readonly selected?: readonly TrackSelector[];
  readonly automatic?: boolean;
  readonly playoutDelays?: readonly {
    readonly publicationId: string;
    readonly playoutDelay: FixedPlayoutDelay;
  }[];
  readonly selectorDelays?: readonly {
    readonly selector: TrackSelector;
    readonly playoutDelay: FixedPlayoutDelay;
  }[];
}

export type PlayoutDelay = { readonly mode: "adaptive" } | FixedPlayoutDelay;

export interface VideoObservation {
  readonly visible: boolean;
  readonly height: number;
  readonly minHeight: number;
  readonly minFps: number;
  readonly priority: number;
  readonly playoutDelay: PlayoutDelay;
}

export interface AudioSubscriptionConfig {
  readonly playoutDelay: PlayoutDelay;
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
 * requested track, not to the connection or other audio selections.
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
  | "participant-superseded"
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
  readonly trackId: string;
  readonly active: boolean;
  watch(listener: () => void): () => void;
  subscribe(configuration?: VideoObservation): VideoSubscription;
}

export interface RemoteAudioTrack {
  readonly kind: "audio";
  readonly id: string | null;
  readonly active: boolean;
  readonly participantId: string;
  readonly trackId: string;
  watch(listener: () => void): () => void;
  subscribe(configuration?: AudioSubscriptionConfig): AudioSubscription;
}

export interface RemoteParticipant {
  readonly room: string;
  readonly participantId: string;
  readonly active: boolean;
  id(): string;
  video(trackId: string): RemoteVideoTrack;
  audio(trackId: string): RemoteAudioTrack;
  data<D extends Delivery>(trackId: string, delivery: D): RemoteDataTrack<D>;
}

export interface WatchConfig {
  readonly limit?: number;
  readonly cursor?: Uint8Array | readonly number[];
}

export type CatalogPublicationKind = CoreCatalogPublicationKind;
export type CatalogPublicationInfo = CoreCatalogPublicationInfo;
export type CatalogParticipantInfo = Omit<
  CoreCatalogParticipantInfo,
  "publications"
> & {
  readonly publications: readonly CatalogPublicationInfo[];
};

export interface CatalogPage {
  readonly revision: bigint;
  readonly participants: readonly CatalogParticipantInfo[];
  readonly hasMore: boolean;
  readonly nextCursor: readonly number[];
  readonly cursorExpiresAtUnixSeconds: bigint;
}

export interface WatchState {
  readonly observationRevision: bigint;
  readonly room: string;
  readonly limit: number;
  readonly cursor: readonly number[];
  readonly status: WatchStatus;
  readonly page?: CatalogPage;
  readonly error?: ErrorInfo;
}

export interface WatchUpdates {
  next(): Promise<WatchState | undefined>;
  close(): void;
}

export interface Watch {
  state(): WatchState;
  updates(): WatchUpdates;
  close(): Promise<void>;
}

export interface RoomRef {
  readonly name: string;
  participant(participantId: string): RemoteParticipant;
  watch(config?: WatchConfig): Watch;
}

export type TopicEvent<T> =
  | {
      readonly type: "message";
      readonly publicationId: string;
      readonly sequence: bigint;
      readonly participantId: string;
      readonly topic: string;
      readonly value: T;
    }
  | {
      readonly type: "gap";
      readonly publicationId: string;
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
  readonly publicationId: string | null;
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

export interface CredentialOutcome {
  readonly revision: bigint;
  readonly status: "accepted" | "rejected" | "uncertain";
  readonly error: AgentError | null;
}

export interface AgentSnapshot {
  readonly version: number;
  readonly desiredRevision: number;
  readonly joinRevision: bigint;
  readonly credentialRevision: bigint;
  readonly acceptedCredentialRevision: bigint | null;
  readonly credentialOutcome: CredentialOutcome | null;
  readonly desiredRoom: string | null;
  readonly connection: ConnectionState;
  readonly generation: number | null;
  readonly participantId: string | null;
  /** Authoritative application room identity, retained during reconnection. */
  readonly roomId: string | null;
  /** Current authorization deadline, in Unix seconds. */
  readonly authorizationExpiresAt: number | null;
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
      readonly publicationId: string;
      readonly sequence: bigint;
      readonly payload: Uint8Array;
    }
  | {
      readonly type: "topic-recovery-gap";
      readonly topic: string;
      readonly participantId: string;
      readonly publicationId: string;
      readonly expectedSequence: bigint;
      readonly availableSequence: bigint;
    }
  | {
      readonly type: "topic-send-admitted";
      readonly topic: string;
      readonly mode: TopicMode;
      readonly operation: bigint;
      readonly publicationId: string;
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

export interface AgentUpdates {
  next(): Promise<Readonly<ObservedAgentState> | undefined>;
  close(): void;
}

export interface Agent {
  /** Current observed connection state. Reactive when returned by React useAgent(). */
  readonly connection: ConnectionState;
  /** Current Agent error, if any. Reactive when returned by React useAgent(). */
  readonly error: AgentError | null;
  readonly participantId: string | null;
  readonly roomId: string | null;
  readonly authorizationExpiresAt: number | null;
  video(trackId: string): LocalVideoTrack;
  audio(trackId: string): LocalAudioTrack;
  data<D extends Delivery>(trackId: string, delivery: D): LocalDataTrack<D>;
  room(name: string): RoomRef;
  selectAudio(policy: AudioDemand): Promise<AudioSelection>;
  /** Synchronously replace the desired room; Core owns admission and recovery. */
  join(room: string): void;
  leaveRoom(): void;
  renew(token: string): void;
  connect(): Promise<void>;
  disconnect(): Promise<void>;
  renewAuthorization(token: string): Promise<void>;
  isClosed(): boolean;
  setLogging(level: LogLevel): void;
  state(): Readonly<ObservedAgentState>;
  updates(): AgentUpdates;
  snapshot(): AgentSnapshot;
  snapshots(): SnapshotStream;
  events(): EventStream;
  close(): Promise<void>;
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
