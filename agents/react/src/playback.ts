import { createElement, useEffect, useRef } from "react";
import type * as React from "react";
import { useCommittedRef } from "./committed-ref.js";
import { validateReceiveOptions } from "@pulsebeam/web";
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

export interface VideoPreferences {
  readonly minHeight?: number;
  readonly minFps?: number;
  readonly priority?: number;
  /** Removing fixed bounds may remap media or recover the transport before defaults can apply. */
  readonly latency?: { readonly minMs: number; readonly maxMs: number };
}

export type VideoProps = Omit<
  React.VideoHTMLAttributes<HTMLVideoElement>,
  "src" | "children"
> & {
  readonly source: VideoSource | null;
  readonly preferences?: VideoPreferences;
  readonly mirror?: boolean;
  readonly onPlaybackError?: PlaybackErrorCallback;
};

export function Video({
  source,
  mirror = false,
  preferences,
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
  const receive = validateReceiveOptions({
    minHeight: preferences?.minHeight,
    minFps: preferences?.minFps,
    priority: preferences?.priority,
    ...(preferences?.latency ? { playoutDelay: preferences.latency } : {}),
  });
  const receiveKey = JSON.stringify(receive);
  const committedReceive = useCommittedRef(receive);

  useEffect(() => {
    const current = element.current;
    if (!current || !source) return;
    const active = attachVideoSource(
      source,
      current,
      (error, retry) => callback.current?.({ error, retry }),
      { autoPlay, preferences: committedReceive.current },
    );
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
