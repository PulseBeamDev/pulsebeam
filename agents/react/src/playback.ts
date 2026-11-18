import { createElement, useEffect, useRef, useState } from "react";
import type * as React from "react";
import type {
  AudioSource as WebAudioSource,
  VideoSource as WebVideoSource,
  AudioSubscription,
  VideoSubscription,
  MediaObservation,
} from "@pulsebeam/web";
import { nativeSourceTrack, watchSource } from "@pulsebeam/web/internal/react";
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
interface Playback {
  setAutoPlay(value: boolean): void;
  close(): Promise<void>;
}

function attach(
  source: Input,
  kind: "video" | "audio",
  element: HTMLMediaElement,
  autoPlay: boolean,
  blocked: (error: unknown, retry: () => Promise<void>) => void,
  failed: (error: unknown) => void,
): Playback {
  let live = true;
  let playing = autoPlay;
  let attempt = 0;
  let revision = 0;
  let stream: MediaStream | undefined;
  let observation: MediaObservation | undefined;
  let pending: Promise<void> | undefined;
  let controller: AbortController | undefined;
  let cursor:
    | ReturnType<VideoSubscription["updates"]>
    | ReturnType<AudioSubscription["updates"]>
    | undefined;
  let unwatch = () => {};
  let unlisten = () => {};
  const releases = new Set<Promise<void>>();
  const trackRelease = (closing: Promise<void>) => {
    releases.add(closing);
    void closing.then(
      () => releases.delete(closing),
      () => releases.delete(closing),
    );
    return closing;
  };
  const detach = () => {
    unlisten();
    unlisten = () => {};
    ++attempt;
    if (element.srcObject === stream) element.srcObject = null;
    stream = undefined;
  };
  const play = async () => {
    const current = stream;
    if (!live || !current) return;
    const ticket = ++attempt;
    try {
      await element.play();
    } catch (error) {
      if (
        !live ||
        stream !== current ||
        ticket !== attempt ||
        (error instanceof DOMException && error.name === "AbortError")
      )
        return;
      const retry = async () => {
        if (live && stream === current) await play();
      };
      blocked(error, retry);
    }
  };
  const present = (track: MediaStreamTrack | null) => {
    detach();
    if (!live || !track || track.kind !== kind || track.readyState !== "live")
      return;
    const ended = () => {
      if ("observe" in source) reconcile();
      else detach();
    };
    track.addEventListener("ended", ended);
    unlisten = () => track.removeEventListener("ended", ended);
    stream = new MediaStream([track]);
    element.srcObject = stream;
    if (playing) void play();
  };
  const retire = () => {
    detach();
    const old = observation;
    observation = undefined;
    if (old) void trackRelease(old.close()).catch(failed);
  };
  const reconcile = () => {
    if (!live || !("observe" in source)) return;
    if (source.isClosed()) {
      retire();
      controller?.abort();
      return;
    }
    if (
      observation &&
      !observation.isClosed() &&
      observation.track()?.readyState === "live"
    )
      return;
    if (observation) retire();
    if (pending) return;
    const start = revision;
    const acquisition = new AbortController();
    controller = acquisition;
    pending = source
      .observe(acquisition.signal)
      .then(async (value) => {
        if (
          !live ||
          acquisition.signal.aborted ||
          source.isClosed() ||
          value.isClosed()
        ) {
          await value.close();
          return;
        }
        observation = value;
        present(value.track());
      })
      .catch((error: unknown) => {
        if (!live || acquisition.signal.aborted || source.isClosed()) return;
        const code =
          typeof error === "object" && error !== null && "code" in error
            ? error.code
            : undefined;
        if (code !== "closed") failed(error);
      })
      .finally(() => {
        pending = undefined;
        if (controller === acquisition) controller = undefined;
        if (live && revision !== start) reconcile();
      });
  };
  try {
    if ("observe" in source) {
      cursor = source.updates();
      void (async () => {
        while (live) {
          const state = await cursor!.next();
          if (!live || state === undefined) return;
          ++revision;
          reconcile();
        }
      })().catch((error: unknown) => {
        if (live) failed(error);
      });
    } else if ("track" in source) {
      present(source.track());
    } else {
      const native = playbackTrack(source, kind);
      unwatch = watchSource(native, () => present(null));
      present(nativeSourceTrack(native, kind));
    }
  } catch (error) {
    failed(error);
  }
  return {
    setAutoPlay(value) {
      if (!live || playing === value) return;
      playing = value;
      ++attempt;
      if (playing && stream) void play();
    },
    close() {
      live = false;
      controller?.abort();
      cursor?.close();
      unwatch();
      detach();
      const old = observation;
      observation = undefined;
      return Promise.allSettled([old?.close(), pending, ...releases]).then(
        () => {},
      );
    },
  };
}

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
    const binding = attach(
      source,
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
