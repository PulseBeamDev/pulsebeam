import { useCallback, useEffect, useState } from "react";
import { useDisplayMedia } from "@pulsebeam/react";
import type { Agent, CaptureResult } from "@pulsebeam/react";

export function useRoomMedia(
  agent: Agent,
  capture: CaptureResult,
  onFailure: (message: string) => void,
  initial: { cameraOn: boolean; micOn: boolean },
) {
  const display = useDisplayMedia({
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
  const camera = agent.localVideoTrack("camera");
  const microphone = agent.localAudioTrack("microphone");
  const screen = agent.localVideoTrack("screen");

  useEffect(() => {
    camera.setSource(cameraOn ? capture.videoTrack : null);
  }, [camera, cameraOn, capture.videoTrack]);
  useEffect(() => {
    microphone.setSource(micOn ? capture.audioTrack : null);
  }, [microphone, micOn, capture.audioTrack]);
  useEffect(() => {
    screen.setSource(display.videoTrack);
  }, [screen, display.videoTrack]);
  useEffect(
    () => () => {
      screen.setSource(null);
      camera.setSource(null);
      microphone.setSource(null);
      display.stop();
    },
    [camera, display.stop, microphone, screen],
  );

  const startShare = useCallback(async () => {
    try {
      await display.request();
    } catch (reason) {
      onFailure(
        reason instanceof Error
          ? `Screen share: ${reason.message}`
          : "Screen sharing was cancelled",
      );
    }
  }, [display.request, onFailure]);
  const toggle = useCallback(
    (slot: "camera" | "microphone", enabled: boolean) => {
      if (slot === "camera") setCameraOn(enabled);
      else setMicOn(enabled);
    },
    [],
  );
  return {
    screen: display.videoTrack,
    camera,
    cameraOn,
    micOn,
    detachScreen: display.stop,
    startShare,
    toggle,
  };
}
