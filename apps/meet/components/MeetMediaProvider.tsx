"use client";

import { createContext, useContext, useState, type ReactNode } from "react";
import { useMediaDevices, useUserMedia } from "@pulsebeam/react";

const MediaContext = createContext<ReturnType<typeof useMeetCapture> | null>(
  null,
);

function useMeetCapture() {
  const [videoDeviceId, setVideoDeviceId] = useState("");
  const [audioDeviceId, setAudioDeviceId] = useState("");
  const capture = useUserMedia({
    video: {
      device: videoDeviceId
        ? { kind: "camera", id: videoDeviceId, label: "" }
        : undefined,
      preferences: { width: 1280, height: 720, frameRate: 30 },
    },
    audio: {
      device: audioDeviceId
        ? { kind: "microphone", id: audioDeviceId, label: "" }
        : undefined,
      preferences: {
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    },
  });
  const devices = useMediaDevices();
  return {
    capture,
    devices,
    videoDeviceId,
    audioDeviceId,
    setVideoDeviceId,
    setAudioDeviceId,
  };
}

export function MeetMediaProvider({ children }: { children: ReactNode }) {
  const media = useMeetCapture();
  return (
    <MediaContext.Provider value={media}>{children}</MediaContext.Provider>
  );
}

export function useMeetMedia() {
  const media = useContext(MediaContext);
  if (!media) throw new Error("Meet media requires MeetMediaProvider");
  return media;
}
