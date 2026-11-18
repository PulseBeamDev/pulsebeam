import { createElement, useEffect, useRef, useState } from "react";
import type * as React from "react";
import type {
  AudioSource as WebAudioSource,
  VideoSource as WebVideoSource,
  AudioSubscription,
  VideoSubscription,
  MediaObservation,
} from "@pulsebeam/web";
import {
  attachMedia,
  type BorrowedMediaAttachment,
} from "@pulsebeam/web/internal/react";
import { useCommittedRef } from "./committed-ref.js";
import {
  playbackTrack,
  type VideoSource,
  type AudioSource,
} from "./sources.js";

export interface PlaybackError {
  readonly error: unknown;
  readonly retry: () => Promise<void>;
}
type PlaybackErrorCallback = (failure: PlaybackError) => void;
type VideoInput =
  | VideoSource
  | WebVideoSource
  | VideoSubscription
  | MediaObservation;
type AudioInput =
  | AudioSource
  | WebAudioSource
  | AudioSubscription
  | MediaObservation;
type Input = VideoInput | AudioInput;
type Playback = BorrowedMediaAttachment;

function usePlayback(
  source: Input | null,
  kind: "video" | "audio",
  autoPlay: boolean,
  onPlaybackError?: PlaybackErrorCallback,
) {
  const element = useRef<HTMLMediaElement>(null);
  const callback = useCommittedRef(onPlaybackError);
  const committedAutoPlay = useCommittedRef(autoPlay);
  const attachment = useRef<Playback | null>(null);
  const [failure, setFailure] = useState<{
    source: Input;
    error: unknown;
  } | null>(null);
  useEffect(() => {
    if (!element.current || !source) return;
    let active = true;
    const binding = attachMedia(
      "observe" in source || "track" in source
        ? source
        : playbackTrack(source, kind),
      kind,
      element.current,
      committedAutoPlay.current,
      (error, retry) => {
        if (active) callback.current?.({ error, retry });
      },
      (error) => {
        if (active) setFailure({ source, error });
      },
    );
    attachment.current = binding;
    return () => {
      active = false;
      if (attachment.current === binding) attachment.current = null;
      void binding
        .close()
        .catch((error: unknown) =>
          console.error("PulseBeam playback cleanup failed", error),
        );
    };
  }, [source, kind]);
  useEffect(() => {
    attachment.current?.setAutoPlay(autoPlay);
  }, [autoPlay]);
  if (failure?.source === source) throw failure.error;
  return element;
}

export type VideoProps = Omit<
  React.VideoHTMLAttributes<HTMLVideoElement>,
  "src" | "children"
> & {
  readonly source: VideoInput | null;
  readonly mirror?: boolean;
  readonly onPlaybackError?: PlaybackErrorCallback;
};
export function Video({
  source,
  mirror = false,
  onPlaybackError,
  muted = true,
  autoPlay = true,
  playsInline = true,
  style,
  ...props
}: VideoProps): React.ReactElement | null {
  const ref = usePlayback(source, "video", autoPlay, onPlaybackError);
  if (!source) return null;
  const presentation = mirror
    ? { ...style, transform: `scaleX(-1) ${style?.transform ?? ""}`.trim() }
    : style;
  return createElement("video", {
    ...props,
    ref,
    muted,
    autoPlay,
    playsInline,
    style: presentation,
  });
}
export type AudioProps = Omit<
  React.AudioHTMLAttributes<HTMLAudioElement>,
  "src" | "children"
> & {
  readonly source: AudioInput | null;
  readonly onPlaybackError?: PlaybackErrorCallback;
};
export function Audio({
  source,
  onPlaybackError,
  autoPlay = true,
  ...props
}: AudioProps): React.ReactElement | null {
  const ref = usePlayback(source, "audio", autoPlay, onPlaybackError);
  if (!source) return null;
  return createElement("audio", { ...props, ref, autoPlay });
}
