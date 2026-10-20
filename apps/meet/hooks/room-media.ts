import { useCallback, useState } from "react";
import { useDisplayMedia } from "@pulsebeam/react";
import type { Agent } from "@pulsebeam/react";

export function useRoomMedia(
  agent: Agent,
  onFailure: (message: string) => void,
  initial: { cameraOn: boolean; micOn: boolean },
) {
  const {
    video: displaySource,
    stop: stopDisplay,
    request: requestDisplay,
  } = useDisplayMedia({
    video: {
      preferences: { width: 1920, height: 1080, frameRate: 30 },
    },
    audio: false,
  });
  const [cameraOn, setCameraOn] = useState(initial.cameraOn);
  const [micOn, setMicOn] = useState(initial.micOn);
  const Camera = agent.local.video("camera");
  const Microphone = agent.local.audio("microphone");
  const Screen = agent.local.video("screen");

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
    screen: displaySource,
    Camera,
    Microphone,
    Screen,
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
