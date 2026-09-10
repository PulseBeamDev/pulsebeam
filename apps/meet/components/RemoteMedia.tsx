import { useRef } from "react";
import type React from "react";
import { useRemoteMedia } from "@pulsebeam/react";
import type { Agent } from "@pulsebeam/react";

export type PlaybackRetry = () => Promise<void>;

export function RemoteMedia({
  agent,
  publicationId,
  kind,
  onBlocked,
}: {
  agent: Agent;
  publicationId: string;
  kind: "audio" | "video";
  onBlocked(reason: string, retry: PlaybackRetry): void;
}) {
  const ref = useRef<HTMLMediaElement>(null);

  useRemoteMedia(agent, ref, {
    publicationIds: [publicationId],
    onPlaybackBlocked: (failure, retry) => onBlocked(failure.message, retry),
  });

  return kind === "video" ? (
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
