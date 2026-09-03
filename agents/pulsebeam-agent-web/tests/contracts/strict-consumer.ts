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
import type {
  WebMediaStream,
  WebMediaTrack,
} from "../generated/bindings/pulsebeam_agent_web";

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
];
