import { useEffect, useMemo, useRef } from "react";
import { Agent, LogLevel, type AgentConfig } from "@pulsebeam/web";
import { sessionKey } from "./session-key.js";
import { useResource } from "./resource.js";

export { usePublish, useSubscribe, useWatch } from "./hooks.js";
export {
  AgentStatus,
  WatchStatus,
  CatalogDelivery,
  DataDelivery,
  DataEvent,
  ErrorCode,
  LogLevel,
  VideoCodec,
} from "@pulsebeam/web";
export type {
  Agent,
  AgentConfig,
  AgentState,
  AgentUpdates,
  AgentError,
  ErrorInfo,
  EffectiveMediaFormat,
  MediaFormatParameter,
  CatalogPublicationInfo,
  CatalogPublicationKind,
  DataPublishOptions,
  DataSendOptions,
  DataSetRef,
  AudioCollection,
  AudioCollectionEntry,
  AudioCollectionRef,
  AudioTrack,
  VideoCollection,
  VideoCollectionEntry,
  VideoCollectionRef,
  VideoTrack,
  PublicationConfig,
  PublicationState,
  TrackSelector,
  VideoObservation,
  VideoPublishOptions,
  VideoSubscriptionState,
  AudioSubscriptionState,
  DataPublicationState,
  DataSubscriptionState,
  LocalAudioTrack,
  LocalVideoTrack,
  LocalDataTrack,
  RemoteAudioTrack,
  RemoteVideoTrack,
  RemoteDataTrack,
  Participant as RemoteParticipant,
  RoomRef,
  Catalog,
  CatalogUpdates,
  CatalogParticipant,
  WatchConfig,
  WatchState,
  VideoPublication,
  AudioPublication,
  DataPublication,
  VideoSubscription,
  AudioSubscription,
  DataSubscription,
  PublicationUpdates,
  VideoSubscriptionUpdates,
  AudioSubscriptionUpdates,
  DataPublicationUpdates,
  DataSubscriptionUpdates,
} from "@pulsebeam/web";
export { Data } from "./ownership.js";
export type { DataProps, PublicationSource } from "./ownership.js";
export { Video, Audio } from "./playback.js";
export type { VideoProps, AudioProps, PlaybackError } from "./playback.js";
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
export function useAgent(config: AgentConfig): Agent | null {
  const key = sessionKey(config);
  const target = useMemo(() => ({ key }), [key]);
  const applied = useRef(new WeakMap<Agent, string>());
  const owner = useResource(target, () => {
    const agent = new Agent(config);
    applied.current.set(agent, config.token);
    return agent;
  });
  const agent = owner.value;

  useEffect(() => {
    if (!agent || applied.current.get(agent) === config.token) return;
    try {
      agent.renew(config.token);
      applied.current.set(agent, config.token);
    } catch (error) {
      owner.fail(error);
    }
  }, [agent, config.token]);
  useEffect(() => {
    try {
      agent?.setLogging(config.logLevel ?? LogLevel.Warn);
    } catch (error) {
      owner.fail(error);
    }
  }, [agent, config.logLevel]);

  return agent;
}
