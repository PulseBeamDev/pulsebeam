import type {
  AgentConfig,
  AgentError,
  DesiredState,
  MediaFrame,
  Notification,
  Snapshot,
  TopicMessage,
  TransportStatistics,
} from "../generated/bindings/pulsebeam_agent_core";
import type * as GeneratedWeb from "../generated/bindings/pulsebeam_agent_web";
import type {
  WebMediaStream,
  WebMediaTrack,
} from "../generated/bindings/pulsebeam_agent_web";

type Assert<T extends true> = T;
type IsAny<T> = 0 extends 1 & T ? true : false;
// Inspect data recursively, not generator implementation helpers or DOM methods.
type HasAny<T> =
  IsAny<T> extends true
    ? true
    : T extends Uint8Array
      ? false
      : T extends (...args: never[]) => unknown
        ? false
        : T extends readonly (infer Value)[]
          ? HasAny<Value>
          : T extends object
            ? true extends { [Key in keyof T]: HasAny<T[Key]> }[keyof T]
              ? true
              : false
            : false;
type NoAny<T> = true extends HasAny<T> ? false : true;
type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false;

// Any in a boundary or nested data field must fail even when assignments compile.
export type StrictBoundaries = [
  Assert<NoAny<AgentConfig>>,
  Assert<NoAny<DesiredState>>,
  Assert<NoAny<Snapshot>>,
  Assert<NoAny<Notification>>,
  Assert<NoAny<TopicMessage>>,
  Assert<NoAny<MediaFrame>>,
  Assert<NoAny<TransportStatistics>>,
  Assert<NoAny<AgentError>>,
  Assert<Equal<WebMediaTrack, MediaStreamTrack>>,
  Assert<Equal<WebMediaStream, MediaStream>>,
  Assert<Equal<MediaFrame["data"], Uint8Array>>,
  Assert<Equal<AgentConfig["token"], string>>,
  Assert<Equal<DesiredState["connected"], boolean>>,
  Assert<Equal<Snapshot["desiredRevision"], bigint>>,
  Assert<Equal<AgentError["message"], string>>,
  Assert<Equal<TransportStatistics["bytesSent"], bigint>>,
  Assert<
    Equal<
      Extract<
        keyof AgentConfig,
        "roomId" | "requestHeaders" | "manualSubscriptions"
      >,
      never
    >
  >,
  Assert<
    Equal<
      Extract<
        keyof typeof import("../generated/bindings/pulsebeam_agent_web"),
        "MediaRegistryProof" | "normalizeAgentConfig"
      >,
      never
    >
  >,
];

// A removed type-only export must not evade the module value-key check.
// @ts-expect-error the proof type is no longer part of the generated boundary
export type RemovedProof = GeneratedWeb.MediaRegistryProof;

// Prove the guard itself rejects both top-level and nested untyped data.
// @ts-expect-error any cannot stand in for a generated contract
export type RejectAny = Assert<NoAny<any>>;
// @ts-expect-error nested any must not escape through a record or collection
export type RejectNestedAny = Assert<NoAny<{ entries: { payload: any }[] }>>;

declare const config: AgentConfig;
declare const desired: DesiredState;
declare const snapshot: Snapshot;
declare const notification: Notification;
declare const topic: TopicMessage;
declare const frame: MediaFrame;
declare const statistics: TransportStatistics;
declare const error: AgentError;
declare const track: MediaStreamTrack;
declare const stream: MediaStream;

const normalized: AgentConfig = config;
const sameTrack: WebMediaTrack = track;
const sameStream: WebMediaStream = stream;
const bytes: Uint8Array = frame.data;
const payload: Uint8Array = topic.inner.payload;
const published: string = desired.publications[0].label;
const externalId: string = snapshot.catalog.participants[0].externalId;
// @ts-expect-error canonical room identity is not client configuration
config.roomId;
// @ts-expect-error arbitrary authentication headers are forbidden
config.requestHeaders;
// @ts-expect-error removed manual-subscription API must stay absent
config.manualSubscriptions;
// @ts-expect-error generated media tracks are native objects, not handles
const invalidTrack: WebMediaTrack = 1n;
void [
  desired,
  snapshot,
  notification,
  topic,
  statistics,
  error,
  normalized,
  sameTrack,
  sameStream,
  bytes,
  payload,
  published,
  externalId,
  invalidTrack,
];
