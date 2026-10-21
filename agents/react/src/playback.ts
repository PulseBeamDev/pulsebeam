import { createElement, useEffect, useRef } from "react";
import type * as React from "react";
import { useCommittedRef } from "./committed-ref.js";
import {
  attachRemoteVideo,
  validateReceiveOptions,
  type ReceiveOptions,
} from "@pulsebeam/web";
import { remoteVideo, type RemoteVideoTrack } from "./agent.js";
import {
  attachVideoSource,
  type VideoAttachment,
  type VideoSource,
} from "./sources.js";

export interface PlaybackError {
  readonly error: unknown;
  readonly retry: () => Promise<void>;
}

type PlaybackErrorCallback = (failure: PlaybackError) => void;

export type VideoProps = Omit<
  React.VideoHTMLAttributes<HTMLVideoElement>,
  "src" | "children"
> & {
  readonly source: VideoSource | RemoteVideoTrack | null;
  readonly receive?: ReceiveOptions;
  readonly mirror?: boolean;
  readonly onPlaybackError?: PlaybackErrorCallback;
};

export function Video({
  source,
  mirror = false,
  receive,
  onPlaybackError,
  muted = true,
  autoPlay = true,
  playsInline = true,
  style,
  ...props
}: VideoProps): React.ReactElement | null {
  const element = useRef<HTMLVideoElement>(null);
  const callback = useCommittedRef(onPlaybackError);
  const attachment = useRef<VideoAttachment | null>(null);
  const validatedReceive = validateReceiveOptions(receive ?? {});
  const receiveKey = JSON.stringify(validatedReceive);
  const committedReceive = useCommittedRef(validatedReceive);

  useEffect(() => {
    const current = element.current;
    if (!current || !source) return;
    const report = (error: unknown, retry: () => Promise<void>) =>
      callback.current?.({ error, retry });
    const active =
      "participantId" in source
        ? attachRemoteVideo(remoteVideo(source), current, report, {
            autoPlay,
            preferences: committedReceive.current,
          })
        : attachVideoSource(source as VideoSource, current, report, {
            autoPlay,
            preferences: committedReceive.current,
          });
    attachment.current = active;
    return () => {
      active.close();
      if (attachment.current === active) attachment.current = null;
    };
  }, [source, autoPlay]);

  useEffect(() => {
    attachment.current?.setPreferences(committedReceive.current);
  }, [receiveKey]);

  if (!source) return null;
  const presentation = mirror
    ? { ...style, transform: `scaleX(-1) ${style?.transform ?? ""}`.trim() }
    : style;
  return createElement("video", {
    ...props,
    ref: element,
    muted,
    autoPlay,
    playsInline,
    style: presentation,
  });
}
