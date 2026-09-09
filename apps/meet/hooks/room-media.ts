import { useCallback, useEffect, useRef, useState } from "react";
import type { Agent } from "@pulsebeam/react";

const sender = {
  camera: { contentHint: "motion" as const },
  microphone: { contentHint: "speech" as const },
  screen: { contentHint: "detail" as const },
};

export const stopMedia = (stream: MediaStream | null) =>
  stream?.getTracks().forEach((track) => track.stop());

export function useRoomMedia(
  agent: Pick<Agent, "replaceLocalTrack" | "setLocalMuted">,
  stream: MediaStream,
  onFailure: (message: string) => void,
) {
  const [screen, setScreen] = useState<MediaStream | null>(null);
  const screenRef = useRef<MediaStream | null>(null);
  const operations = useRef(Promise.resolve());
  const alive = useRef(true);
  const captureRequest = useRef(0);
  const [cameraOn, setCameraOn] = useState(
    stream.getVideoTracks()[0]?.enabled ?? false,
  );
  const [micOn, setMicOn] = useState(
    stream.getAudioTracks()[0]?.enabled ?? false,
  );

  const queue = useCallback(
    (operation: () => Promise<void>) => {
      operations.current = operations.current
        .then(operation, operation)
        .catch((reason) => {
          if (alive.current) {
            onFailure(
              reason instanceof Error
                ? reason.message
                : "Media operation failed",
            );
          }
        });
    },
    [onFailure],
  );

  useEffect(() => {
    queue(async () => {
      const camera = stream.getVideoTracks()[0] ?? null;
      const microphone = stream.getAudioTracks()[0] ?? null;
      if (camera) camera.enabled = cameraOn;
      if (microphone) microphone.enabled = micOn;
      await agent.replaceLocalTrack("camera", camera, sender.camera);
      await agent.replaceLocalTrack(
        "microphone",
        microphone,
        sender.microphone,
      );
      await agent.setLocalMuted("camera", !cameraOn);
      await agent.setLocalMuted("microphone", !micOn);
    });
  }, [agent, cameraOn, micOn, queue, stream]);

  const detachScreen = useCallback(() => {
    const active = screenRef.current;
    if (!active) return;
    screenRef.current = null;
    setScreen(null);
    queue(async () => {
      try {
        await agent.replaceLocalTrack("screen", null, sender.screen);
      } finally {
        stopMedia(active);
      }
    });
  }, [agent, queue]);

  const startShare = useCallback(async () => {
    const request = ++captureRequest.current;
    try {
      const Capture = (
        globalThis as typeof globalThis & {
          CaptureController?: new () => object;
        }
      ).CaptureController;
      const controller = Capture ? new Capture() : undefined;
      const display = await navigator.mediaDevices.getDisplayMedia({
        video: {
          width: { ideal: 1920 },
          height: { ideal: 1080 },
          frameRate: { ideal: 30 },
          displaySurface: "monitor",
        },
        audio: false,
        systemAudio: "exclude",
        windowAudio: "exclude",
        surfaceSwitching: "include",
        ...(controller ? { controller } : {}),
      } as MediaStreamConstraints);
      if (
        !alive.current ||
        request !== captureRequest.current ||
        screenRef.current
      ) {
        stopMedia(display);
        return;
      }
      screenRef.current = display;
      setScreen(display);
      display
        .getVideoTracks()[0]
        ?.addEventListener("ended", detachScreen, { once: true });
      queue(async () => {
        try {
          await agent.replaceLocalTrack(
            "screen",
            display.getVideoTracks()[0] ?? null,
            sender.screen,
          );
        } catch (reason) {
          if (screenRef.current === display) detachScreen();
          throw reason;
        }
      });
    } catch (reason) {
      if (alive.current && request === captureRequest.current) {
        onFailure(
          reason instanceof Error
            ? `Screen share: ${reason.message}`
            : "Screen sharing was cancelled",
        );
      }
    }
  }, [agent, detachScreen, onFailure, queue]);

  useEffect(() => {
    const requests = captureRequest;
    alive.current = true;
    return () => {
      alive.current = false;
      ++requests.current;
      detachScreen();
    };
  }, [detachScreen]);

  const toggle = useCallback(
    (slot: "camera" | "microphone", enabled: boolean) => {
      stream
        .getTracks()
        .filter(
          (track) => track.kind === (slot === "camera" ? "video" : "audio"),
        )
        .forEach((track) => {
          track.enabled = enabled;
        });
      if (slot === "camera") setCameraOn(enabled);
      else setMicOn(enabled);
      queue(() => agent.setLocalMuted(slot, !enabled));
    },
    [agent, queue, stream],
  );

  return { screen, cameraOn, micOn, detachScreen, startShare, toggle };
}
