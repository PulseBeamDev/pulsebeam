import { createElement, useEffect, useRef, useState } from "react";
import type * as React from "react";
import {
  AudioTrack,
  VideoTrack,
  type AudioSource as WebAudioSource,
  type VideoSource as WebVideoSource,
  type AudioSubscription,
  type VideoSubscription,
} from "@pulsebeam/web";
import {
  attachMedia,
  type BorrowedMediaAttachment,
} from "@pulsebeam/web/internal/react";
import { useCommittedRef } from "./committed-ref.js";
import { useStableTarget } from "./stable-target.js";
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
type VideoInput = VideoSource | WebVideoSource | VideoSubscription | VideoTrack;
type AudioInput = AudioSource | WebAudioSource | AudioSubscription | AudioTrack;
type Input = VideoInput | AudioInput;
type Playback = BorrowedMediaAttachment;

function sameAdmission(previous: Input | null, current: Input | null) {
  if (previous instanceof VideoTrack && current instanceof VideoTrack)
    return previous.sameAdmission(current);
  if (previous instanceof AudioTrack && current instanceof AudioTrack)
    return previous.sameAdmission(current);
  return false;
}

function usePlayback(
  input: Input | null,
  kind: "video" | "audio",
  autoPlay: boolean,
  onPlaybackError?: PlaybackErrorCallback,
) {
  const source = useStableTarget(input, sameAdmission);
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
      "playbackOutput" in source || "previewTrack" in source
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
