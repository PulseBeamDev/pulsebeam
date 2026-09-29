import { useEffect, useState, useSyncExternalStore } from "react";
import { createAgent, type Agent, type AgentConfig } from "@pulsebeam/web";

export { createAgent, LocalTrackCapacityError } from "@pulsebeam/web";
export { Video, Audio } from "./playback.js";
export type { AudioProps, VideoProps, PlaybackError } from "./playback.js";
export {
  useMediaDevices,
  useUserMedia,
  useDisplayMedia,
  MediaCaptureError,
} from "./capture.js";
export type {
  CaptureResult,
  DisplayMediaOptions,
  MediaCaptureErrorCode,
  MediaDevice,
  MediaDevicesResult,
  UserMediaOptions,
} from "./capture.js";
export type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentFailure,
  AgentLogging,
  AgentSnapshot,
  AgentState,
  AudioDemand,
  CatalogSnapshot,
  AudioSenderConfig,
  ConnectionState,
  FailureClass,
  FixedPlayoutDelay,
  MediaKind,
  MappingSnapshot,
  MappedRemoteAudioTrack,
  MappedRemoteVideoTrack,
  MediaTopology,
  LogLevel,
  LocalAudioTrack,
  LocalVideoTrack,
  CapturedAudioTrack,
  CapturedVideoTrack,
  Participant,
  Publication,
  PublicationIntent,
  RemoteAudioSource,
  RemoteAudioTrack,
  ReceiveOptions,
  RemoteTrack,
  RemoteVideoTrack,
  SenderConfig,
  SenderEncoding,
  TopicDropReason,
  TopicMode,
  TopicPublisherStatus,
  TopicRegistration,
  TopicSnapshot,
  TopicSubscriberStatus,
  Topic,
  TrackMapping,
  TrackSelector,
  VideoDemand,
  VideoSenderConfig,
} from "@pulsebeam/web";

const noSubscription = () => () => {};
const noSnapshot = () => null;

export function useAgent(config: AgentConfig): Agent | null {
  const [owned, setOwned] = useState<{ key: string; agent: Agent } | null>(
    null,
  );
  const key = JSON.stringify([
    config.endpoint,
    config.topology,
    config.logging,
  ]);

  useEffect(() => {
    const agent = createAgent(config);
    setOwned({ key, agent });
    return () => agent.close();
  }, [key]);
  useEffect(() => {
    if (owned?.key === key) owned.agent.renewAuthorization(config.token);
  }, [config.token, key, owned]);

  const agent = owned?.key === key ? owned.agent : null;
  useSyncExternalStore(
    agent?.subscribe ?? noSubscription,
    agent?.getSnapshot ?? noSnapshot,
    agent?.getSnapshot ?? noSnapshot,
  );
  return agent;
}
