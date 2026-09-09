import { useEffect, useRef } from "react";
import type React from "react";
import type { RemoteTrack } from "@pulsebeam/react";

export type PlaybackRetry = () => void;

export function RemoteMedia({
  track,
  onBlocked,
}: {
  track: RemoteTrack;
  onBlocked(reason: string, retry: PlaybackRetry): void;
}) {
  const ref = useRef<HTMLMediaElement>(null);

  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    const retry = () => {
      void element
        .play()
        .catch((reason) =>
          onBlocked(
            reason instanceof Error ? reason.message : "Playback was blocked",
            retry,
          ),
        );
    };
    element.srcObject = new MediaStream([track.media]);
    retry();
    return () => {
      element.pause();
      element.srcObject = null;
    };
  }, [onBlocked, track]);

  return track.kind === "video" ? (
    <video
      ref={ref as React.RefObject<HTMLVideoElement>}
      autoPlay
      muted
      playsInline
      className="h-full w-full object-contain"
    />
  ) : (
    <audio ref={ref as React.RefObject<HTMLAudioElement>} autoPlay />
  );
}
