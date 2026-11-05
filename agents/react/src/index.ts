import { useEffect, useState, useSyncExternalStore } from "react";
import { sessionKey } from "./session-key.js";
import { createReactAgent, type Agent } from "./agent.js";
import type { AgentConfig } from "@pulsebeam/web";
import {
  createManagedAgent,
  type ManagedAgent,
} from "@pulsebeam/web/internal/react";

export type {
  AgentConfig,
  AudioDemand,
  AgentError,
  AgentLogging,
  ConnectionState,
  DataEvent,
  DataGap,
  DataMessage,
  Delivery,
  FailureClass,
  LogLevel,
  PlaybackFailure,
  ReceiveOptions,
} from "@pulsebeam/web";
export type {
  Agent,
  LocalAudioTrack,
  LocalVideoTrack,
  LocalDataTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  RemoteParticipant,
  RemoteTrack,
  RemoteVideoTrack,
} from "./agent.js";
export { Publish, Data, Audio, SelectAudio } from "./ownership.js";
export type {
  AudioProps,
  SelectAudioProps,
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
  const [failure, setFailure] = useState<{
    managed: ManagedAgent;
    error: unknown;
  } | null>(null);
  useEffect(() => {
    managed.activate();
    return () => managed.deactivate();
  }, [managed]);
  useEffect(() => {
    let active = true;
    void managed.renewAuthorization(config.token).catch((error: unknown) => {
      if (active) setFailure({ managed, error });
    });
    return () => {
      active = false;
    };
  }, [managed, config.token]);
  useEffect(() => {
    managed.setLogging(config.logging?.level ?? "warn");
  }, [managed, config.logging?.level]);
  useSyncExternalStore(
    managed.subscribe,
    managed.getSnapshot,
    managed.getSnapshot,
  );
  if (failure?.managed === managed) throw failure.error;
  return facade;
}
