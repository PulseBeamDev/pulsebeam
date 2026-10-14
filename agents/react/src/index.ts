import { useEffect, useState, useSyncExternalStore } from "react";
import {
  createManagedAgent,
  type Agent,
  type AgentConfig,
} from "@pulsebeam/web";

export { createAgent, LocalTrackCapacityError } from "@pulsebeam/web";
export { Video } from "./playback.js";
export type { VideoProps, PlaybackError } from "./playback.js";
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
  LocalMedia,
  RemoteMedia,
  RemoteParticipant,
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

/**
 * One React-owned Agent handle per endpoint/topology configuration.
 * Construction is inert during render; effects activate the runtime after commit.
 * Use a React key when the room or participant changes.
 */
export function useAgent(config: AgentConfig): Agent {
  const key = JSON.stringify([
    config.endpoint,
    config.topology?.localVideos ?? 2,
    config.topology?.localAudios ?? 2,
    config.topology?.remoteVideos ?? 16,
    config.topology?.remoteAudios ?? 8,
  ]);

  const [owned, setOwned] = useState(() => ({
    key,
    agent: createManagedAgent(config),
  }));
  // A changed immutable configuration creates a new inert handle. React retries
  // this render before committing children, avoiding a stale-session render.
  let current = owned;
  if (owned.key !== key) {
    current = { key, agent: createManagedAgent(config) };
    setOwned(current);
  }
  const agent = current.agent;

  useEffect(() => {
    agent.activate();
    return () => agent.deactivate();
  }, [agent]);

  useEffect(() => {
    agent.renewAuthorization(config.token);
  }, [agent, config.token]);

  useEffect(() => {
    agent.setLogging(config.logging?.level ?? "warn");
  }, [agent, config.logging?.level]);

  useSyncExternalStore(agent.subscribe, agent.getSnapshot, agent.getSnapshot);
  return agent;
}
