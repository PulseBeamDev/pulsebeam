import { useEffect, useRef, useState, useSyncExternalStore } from "react";
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
export function useAgent(config: AgentConfig): Agent | null {
  const key = sessionKey(config);
  const [owned, setOwned] = useState<{
    key: string;
    managed: ManagedAgent;
    facade: Agent;
  } | null>(null);
  const [failure, setFailure] = useState<{
    key: string;
    lifetime: number;
    error: unknown;
  } | null>(null);
  const generation = useRef(0);
  const cleanup = useRef<Promise<void>>(Promise.resolve());
  const current = owned?.key === key ? owned : null;

  useEffect(() => {
    const lifetime = ++generation.current;
    let active = true;
    let managed: ManagedAgent | undefined;
    const reportCleanup = (error: unknown) => {
      console.error("PulseBeam Agent cleanup failed", error);
    };
    void (async () => {
      await cleanup.current;
      if (!active || generation.current !== lifetime) return;
      try {
        managed = createManagedAgent(config);
        managed.activate();
        await managed.initialized();
        if (!active || generation.current !== lifetime) {
          const retired = managed;
          managed = undefined;
          await retired.close();
          return;
        }
        setOwned({ key, managed, facade: createReactAgent(managed) });
      } catch (error) {
        if (managed) {
          try {
            await managed.close();
          } catch (closeError) {
            reportCleanup(closeError);
          }
        }
        if (active && generation.current === lifetime)
          setFailure({ key, lifetime, error });
      }
    })();
    return () => {
      active = false;
      if (generation.current === lifetime) ++generation.current;
      if (managed) {
        const retired = managed;
        managed = undefined;
        cleanup.current = cleanup.current
          .then(() => retired.close())
          .catch((error: unknown) => {
            reportCleanup(error);
          });
      }
    };
  }, [key]);

  useEffect(() => {
    if (!current) return;
    try {
      current.managed.renew(config.token);
    } catch (error) {
      setFailure({ key, lifetime: generation.current, error });
    }
  }, [current?.managed, config.token, key]);
  useEffect(() => {
    current?.managed.setLogging(config.logging?.level ?? "warn");
  }, [current?.managed, config.logging?.level]);

  const subscribe = current?.managed.subscribe ?? noSubscribe;
  const getSnapshot = current?.managed.getSnapshot ?? noSnapshot;
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  if (failure?.key === key && failure.lifetime === generation.current)
    throw failure.error;
  return current?.facade ?? null;
}

const noSubscribe = (_listener: () => void) => () => {};
const noSnapshot = () => null;
