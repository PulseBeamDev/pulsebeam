import { useEffect, useRef, useState } from "react";
import { Agent, type AgentConfig } from "@pulsebeam/web";
import { sessionKey } from "./session-key.js";
import { useObservation } from "./observation.js";

export { usePublish, useSubscribe, useWatch } from "./hooks.js";
export { WatchStatus, CatalogDelivery, DataDelivery } from "@pulsebeam/web";
export type {
  Agent,
  AgentConfig,
  AgentState,
  AgentUpdates,
  AudioDemand,
  AgentError,
  AgentLogging,
  ConnectionState,
  DataEvent,
  DataDiscontinuity,
  DataPublishOptions,
  DataSubscribeOptions,
  DataMessage,
  DataUpdate,
  Delivery,
  FailureClass,
  LogLevel,
  PlaybackFailure,
  ReceiveOptions,
  ErrorInfo,
  LocalAudioTrack,
  LocalVideoTrack,
  LocalDataTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  RemoteParticipant,
  RoomRef,
  Watch,
  WatchConfig,
  WatchState,
  WatchUpdates,
  CatalogPage,
  CatalogParticipantInfo,
  CatalogPublicationInfo,
  CatalogPublicationKind,
  RemoteVideoTrack,
  AudioPublication,
  VideoPublication,
  DataPublication,
  AudioSubscription,
  VideoSubscription,
  DataSubscription,
  PublicationConfig,
  VideoObservation,
  AudioSubscriptionConfig,
  PublicationState,
  VideoSubscriptionState,
  AudioSubscriptionState,
  DataPublicationState,
  DataSubscriptionState,
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
  const [owned, setOwned] = useState<{
    key: string;
    lifetime: number;
    agent: Agent;
  } | null>(null);
  const [failure, setFailure] = useState<{
    key: string;
    lifetime: number;
    error: unknown;
  } | null>(null);
  const generation = useRef(0);
  const cleanup = useRef<Promise<void>>(Promise.resolve());
  const current =
    owned?.key === key && owned.lifetime === generation.current ? owned : null;

  useEffect(() => {
    const lifetime = ++generation.current;
    let active = true;
    let agent: Agent | undefined;
    const reportCleanup = (error: unknown) => {
      console.error("PulseBeam Agent cleanup failed", error);
    };
    void (async () => {
      await cleanup.current;
      if (!active || generation.current !== lifetime) return;
      try {
        agent = new Agent(config);
        setOwned({ key, lifetime, agent });
      } catch (error) {
        if (active && generation.current === lifetime)
          setFailure({ key, lifetime, error });
      }
    })();
    return () => {
      active = false;
      if (generation.current === lifetime) ++generation.current;
      if (agent) {
        const closing = agent.close();
        agent = undefined;
        cleanup.current = Promise.all([cleanup.current, closing])
          .then(() => {})
          .catch(reportCleanup);
      }
    };
  }, [key]);

  useEffect(() => {
    if (!current) return;
    try {
      current.agent.renew(config.token);
    } catch (error) {
      setFailure({ key, lifetime: generation.current, error });
    }
  }, [current?.agent, config.token, key]);
  useEffect(() => {
    current?.agent.setLogging(config.logging?.level ?? "warn");
  }, [current?.agent, config.logging?.level]);

  useObservation(current?.agent ?? null);
  if (failure?.key === key && failure.lifetime === generation.current)
    throw failure.error;
  return current?.agent ?? null;
}
