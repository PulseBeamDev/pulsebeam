"use client";

import { createContext, useContext, useState, type ReactNode } from "react";
import { useCapture, useDevices } from "@pulsebeam/react";

const MediaContext = createContext<ReturnType<typeof useMeetCapture> | null>(
  null,
);

function useMeetCapture() {
  const [videoDeviceId, setVideoDeviceId] = useState("");
  const [audioDeviceId, setAudioDeviceId] = useState("");
  const [cameraOn, setCameraOn] = useState(true);
  const [micOn, setMicOn] = useState(true);
  const capture = useCapture("device", {
    video: {
      device: videoDeviceId
        ? { kind: "camera", id: videoDeviceId, label: "" }
        : undefined,
      width: 1280,
      height: 720,
      frameRate: 30,
    },
    audio: {
      device: audioDeviceId
        ? { kind: "microphone", id: audioDeviceId, label: "" }
        : undefined,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
  });
  const devices = useDevices();
  return {
    capture,
    devices,
    videoDeviceId,
    audioDeviceId,
    setVideoDeviceId,
    setAudioDeviceId,
    cameraOn,
    micOn,
    setCameraOn,
    setMicOn,
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
