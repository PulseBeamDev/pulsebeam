"use client";

import { createContext, useContext, useState } from "react";
import type { ReactNode } from "react";
import { useMediaDevices, useUserMedia } from "@pulsebeam/react";
import type { CaptureResult, MediaDevicesResult } from "@pulsebeam/react";

type MeetMedia = {
  capture: CaptureResult;
  devices: MediaDevicesResult;
  videoDeviceId: string;
  audioDeviceId: string;
  setVideoDeviceId(id: string): void;
  setAudioDeviceId(id: string): void;
};

const MediaContext = createContext<MeetMedia | null>(null);

export function MeetMediaProvider({ children }: { children: ReactNode }) {
  const [videoDeviceId, setVideoDeviceId] = useState("");
  const [audioDeviceId, setAudioDeviceId] = useState("");
  const capture = useUserMedia({
    video: {
      deviceId: videoDeviceId ? { exact: videoDeviceId } : undefined,
      width: { ideal: 1280 },
      height: { ideal: 720 },
      frameRate: { ideal: 30 },
    },
    audio: {
      deviceId: audioDeviceId ? { exact: audioDeviceId } : undefined,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
  });
  const devices = useMediaDevices();
  return (
    <MediaContext.Provider
      value={{
        capture,
        devices,
        videoDeviceId,
        audioDeviceId,
        setVideoDeviceId,
        setAudioDeviceId,
      }}
    >
      {children}
    </MediaContext.Provider>
  );
}

export function useMeetMedia(): MeetMedia {
  const media = useContext(MediaContext);
  if (!media) throw new Error("Meet media requires MeetMediaProvider");
  return media;
}
