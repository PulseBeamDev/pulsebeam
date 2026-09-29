import {
  createElement,
  useCallback,
  useEffect,
  useRef,
  useSyncExternalStore,
} from "react";
import type * as React from "react";
import { useCommittedRef } from "./committed-ref.js";
import {
  attachRemoteAudio,
  attachRemoteVideo,
  nativeCaptureTrack,
  type CapturedVideoTrack,
  type LocalVideoTrack,
  type PlaybackFailure,
  type RemoteAudioSource,
  type RemoteVideoTrack,
} from "@pulsebeam/web";

export interface PlaybackError {
  readonly error: unknown;
  readonly retry: () => Promise<void>;
}

type PlaybackErrorCallback = (failure: PlaybackError) => void;

export type VideoProps = Omit<
  React.VideoHTMLAttributes<HTMLVideoElement>,
  "src" | "children"
> & {
  readonly source:
    | CapturedVideoTrack
    | LocalVideoTrack
    | RemoteVideoTrack
    | null;
  readonly mirror?: boolean;
  readonly onPlaybackError?: PlaybackErrorCallback;
};

export type AudioProps = Omit<
  React.AudioHTMLAttributes<HTMLAudioElement>,
  "src" | "children"
> & {
  readonly source: RemoteAudioSource | null;
  readonly onPlaybackError?: PlaybackErrorCallback;
};

function report(
  callback: PlaybackErrorCallback | undefined,
  failure: PlaybackFailure,
  retry: () => Promise<void>,
): void {
  callback?.({ error: failure.cause ?? new Error(failure.message), retry });
}

function isRemote(
  source: CapturedVideoTrack | LocalVideoTrack | RemoteVideoTrack | null,
): source is RemoteVideoTrack {
  return source !== null && "setReceiveOptions" in source;
}

export function Video({
  source,
  mirror = false,
  onPlaybackError,
  muted = true,
  playsInline = true,
  style,
  ...props
}: VideoProps): React.ReactElement | null {
  const element = useRef<HTMLVideoElement>(null);
  const callback = useCommittedRef(onPlaybackError);
  const remote = isRemote(source) ? source : null;
  const local = source && "source" in source ? source : null;
  const captured =
    source && !remote && !local ? (source as CapturedVideoTrack) : null;
  const subscribeRemote = useCallback(
    (listener: () => void) => remote?.subscribe(listener) ?? (() => {}),
    [remote],
  );
  const subscribeLocal = useCallback(
    (listener: () => void) => local?.subscribe(listener) ?? (() => {}),
    [local],
  );
  const active = useSyncExternalStore(
    subscribeRemote,
    () => remote?.active ?? false,
    () => false,
  );
  const localSource = useSyncExternalStore(
    subscribeLocal,
    () => local?.source ?? captured,
    () => null,
  );

  useEffect(() => {
    const current = element.current;
    if (!current || !remote || !active) return;
    const attachment = attachRemoteVideo(remote, current, (failure, retry) =>
      report(callback.current, failure, retry),
    );
    return () => attachment.close();
  }, [remote, active]);

  useEffect(() => {
    const current = element.current;
    if (!current || !localSource) return;
    const stream = new MediaStream([nativeCaptureTrack(localSource, "video")]);
    current.srcObject = stream;
    let cancelled = false;
    const retry = async () => {
      if (!cancelled) await current.play();
    };
    void retry().catch((error: unknown) => {
      if (!cancelled) callback.current?.({ error, retry });
    });
    return () => {
      cancelled = true;
      if (current.srcObject === stream) current.srcObject = null;
    };
  }, [local, localSource]);

  if (!source || (remote && !active)) return null;
  const presentation = mirror
    ? { ...style, transform: `scaleX(-1) ${style?.transform ?? ""}`.trim() }
    : style;
  return createElement("video", {
    ...props,
    ref: element,
    muted,
    playsInline,
    style: presentation,
  });
}

export function Audio({
  source,
  onPlaybackError,
  autoPlay = true,
  ...props
}: AudioProps): React.ReactElement | null {
  const element = useRef<HTMLAudioElement>(null);
  const callback = useCommittedRef(onPlaybackError);
  useEffect(() => {
    const current = element.current;
    if (!current || !source) return;
    const attachment = attachRemoteAudio(source, current, (failure, retry) =>
      report(callback.current, failure, retry),
    );
    return () => attachment.close();
  }, [source]);
  if (!source) return null;
  return createElement("audio", { ...props, ref: element, autoPlay });
}
