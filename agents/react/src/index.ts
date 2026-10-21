import { useEffect, useState, useSyncExternalStore } from "react";
import { sessionKey } from "./session-key.js";
import { createReactAgent, type Agent } from "./agent.js";
import type { AgentConfig } from "@pulsebeam/web";
import { createManagedAgent } from "@pulsebeam/web/internal/react";

export { track } from "@pulsebeam/web";
export type {
  AgentConfig,
  AgentError,
  AgentFailure,
  AgentLogging,
  AudioTrackSpec,
  ConnectionState,
  DataEvent,
  DataGap,
  DataMessage,
  DataTrackSpec,
  Delivery,
  FailureClass,
  LocalAudioTrack,
  LocalVideoTrack,
  LogLevel,
  MediaTopology,
  PlaybackFailure,
  ReceiveOptions,
  TrackSpec,
  VideoTrackSpec,
} from "@pulsebeam/web";
export type {
  Agent,
  LocalDataTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  RemoteMedia,
  RemoteParticipant,
  RemoteTrack,
  RemoteVideoTrack,
} from "./agent.js";
export { Publish, Data, Audio } from "./ownership.js";
export type {
  AudioProps,
  DataProps,
  PublicationSource,
  PublishProps,
} from "./ownership.js";
export { Video } from "./playback.js";
export type { VideoProps, PlaybackError } from "./playback.js";
export { useDevices, useCapture, MediaCaptureError } from "./capture.js";
export type {
  AudioCaptureSettings,
  CameraDevice,
  CapturedAudio,
  CapturedVideo,
  CaptureResult,
  DeviceCapture,
  DevicesResult,
  DisplayCapture,
  MediaCaptureErrorCode,
  MicrophoneDevice,
  VideoCaptureSettings,
} from "./capture.js";

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
