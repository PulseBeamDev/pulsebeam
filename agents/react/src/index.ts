import { useEffect, useState, useSyncExternalStore } from "react";
import { sessionKey } from "./session-key.js";
import { createReactAgent, type Agent } from "./relationships.js";
import type { AgentConfig } from "@pulsebeam/web";
import { createManagedAgent } from "@pulsebeam/web/internal/react";

export {
  LocalTrackCapacityError,
  RelationshipOwnershipError,
} from "@pulsebeam/web";
export { Video } from "./playback.js";
export type {
  VideoProps,
  VideoPreferences,
  PlaybackError,
} from "./playback.js";
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
  CameraDevice,
  MicrophoneDevice,
  SpeakerDevice,
  VideoCaptureOptions,
  AudioCaptureOptions,
  VideoCapturePreferences,
  AudioCapturePreferences,
  VideoCaptureSettings,
  AudioCaptureSettings,
  CapturedVideo,
  CapturedAudio,
  MediaDevicesResult,
  UserMediaOptions,
} from "./capture.js";
export type {
  Agent,
  LocalVideo,
  LocalAudio,
  Topic,
  RemoteMedia,
  RemoteParticipant,
  RemoteVideoTrack,
  RemoteAudioTrack,
  VideoPublicationProps,
  AudioPublicationProps,
  VideoPublicationPreferences,
  AudioPublicationPreferences,
} from "./relationships.js";
export type {
  VideoSource,
  AudioSource,
  PublishableVideoSource,
  PublishableAudioSource,
} from "./sources.js";
export type {
  AgentConfig,
  AgentError,
  AgentFailure,
  AgentLogging,
  ConnectionState,
  FailureClass,
  MediaKind,
  MediaTopology,
  LogLevel,
  TopicCallbacks,
  TopicConsumerError,
  TopicGap,
  TopicEvent,
  TopicMode,
} from "@pulsebeam/web";

/** React owns activation and release. Key the component for a different logical participant. */
export function useAgent(config: AgentConfig): Agent {
  const key = sessionKey(config);
  const create = () => {
    const managed = createManagedAgent(config);
    return { key, managed, facade: createReactAgent(managed) };
  };
  const [owned, setOwned] = useState(create);
  let current = owned;
  if (owned.key !== key) {
    current = create();
    setOwned(current);
  }
  const { managed, facade } = current;
  useEffect(() => {
    managed.activate();
    return () => managed.deactivate();
  }, [managed]);
  useEffect(() => {
    managed.renewAuthorization(config.token);
  }, [managed, config.token]);
  useEffect(() => {
    managed.setLogging(config.logging?.level ?? "warn");
  }, [managed, config.logging?.level]);
  useSyncExternalStore(
    managed.subscribe,
    managed.getSnapshot,
    managed.getSnapshot,
  );
  return facade;
}
