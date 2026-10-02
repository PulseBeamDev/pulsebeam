import { useCallback, useEffect, useState } from "react";
import { useDisplayMedia } from "@pulsebeam/react";
import type { Agent, CaptureResult } from "@pulsebeam/react";

export type MediaSources = Pick<CaptureResult, "videoTrack" | "audioTrack">;

export function useRoomMedia(
  agent: Agent,
  sources: MediaSources,
  onFailure: (message: string) => void,
  initial: { cameraOn: boolean; micOn: boolean },
) {
  const {
    videoTrack: displayTrack,
    stop: stopDisplay,
    request: requestDisplay,
  } = useDisplayMedia({
    video: {
      width: { ideal: 1920 },
      height: { ideal: 1080 },
      frameRate: { ideal: 30 },
      displaySurface: "monitor",
    },
    audio: false,
  });
  const [cameraOn, setCameraOn] = useState(initial.cameraOn);
  const [micOn, setMicOn] = useState(initial.micOn);
  const camera = agent.local.video("camera");
  const microphone = agent.local.audio("microphone");
  const screen = agent.local.video("screen");

  useEffect(() => {
    camera.setSource(cameraOn ? sources.videoTrack : null);
  }, [camera, cameraOn, sources.videoTrack]);
  useEffect(() => {
    microphone.setSource(micOn ? sources.audioTrack : null);
  }, [microphone, micOn, sources.audioTrack]);
  useEffect(() => {
    screen.setSource(displayTrack);
  }, [screen, displayTrack]);
  useEffect(
    () => () => {
      screen.setSource(null);
      camera.setSource(null);
      microphone.setSource(null);
      stopDisplay();
    },
    [camera, stopDisplay, microphone, screen],
  );

  const startShare = useCallback(async () => {
    try {
      await requestDisplay();
    } catch (reason) {
      onFailure(
        reason instanceof Error
          ? `Screen share: ${reason.message}`
          : "Screen sharing was cancelled",
      );
    }
  }, [requestDisplay, onFailure]);
  return {
    screen: displayTrack,
    camera,
    cameraOn,
    micOn,
    detachScreen: stopDisplay,
    startShare,
    toggle(slot: "camera" | "microphone", enabled: boolean) {
      if (slot === "camera") setCameraOn(enabled);
      else setMicOn(enabled);
    },
  };
}
