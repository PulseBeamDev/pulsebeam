import {
  createElement,
  useCallback,
  useEffect,
  useRef,
  useSyncExternalStore,
} from "react";
import type * as React from "react";
import { useCommittedRef } from "./committed-ref.js";
import { localVideoView, type LocalVideo } from "./relationships.js";
import {
  remoteVideoSource,
  type RemoteVideoTrack as ReactRemoteVideoTrack,
} from "./remote.js";
import {
  attachRemoteVideo,
  nativeCaptureTrack,
  validateReceiveOptions,
  type RemoteVideoAttachment,
  type ReceiveOptions,
  type CapturedVideoTrack,
  type PlaybackFailure,
} from "@pulsebeam/web";

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
  readonly source:
    | CapturedVideoTrack
    | LocalVideo
    | ReactRemoteVideoTrack
    | null;
  readonly preferences?: VideoPreferences;
  readonly mirror?: boolean;
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
  source: VideoProps["source"],
): source is ReactRemoteVideoTrack {
  return source !== null && "participantId" in source;
}

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
  const remote = isRemote(source) ? source : null;
  const local = localVideoView(source);
  const attachment = useRef<RemoteVideoAttachment | null>(null);
  const receive: ReceiveOptions = validateReceiveOptions({
    minHeight: preferences?.minHeight,
    minFps: preferences?.minFps,
    priority: preferences?.priority,
    ...(preferences?.latency ? { playoutDelay: preferences.latency } : {}),
  });
  const receiveKey = JSON.stringify(receive);
  const committedReceive = useCommittedRef(receive);
  const captured =
    source && !remote && !local ? (source as CapturedVideoTrack) : null;
  const subscribeLocal = useCallback(
    (listener: () => void) => local?.subscribe(listener) ?? (() => {}),
    [local],
  );
  const localSource = useSyncExternalStore(
    subscribeLocal,
    () => (local ? (local.enabled ? local.source : null) : captured),
    () => null,
  );

  useEffect(() => {
    const current = element.current;
    if (!current || !remote) return;
    const active = attachRemoteVideo(
      remoteVideoSource(remote),
      current,
      (failure, retry) => report(callback.current, failure, retry),
      { autoPlay, preferences: committedReceive.current },
    );
    attachment.current = active;
    return () => {
      active.close();
      if (attachment.current === active) attachment.current = null;
    };
  }, [remote, autoPlay]);

  useEffect(() => {
    attachment.current?.setPreferences(committedReceive.current);
  }, [receiveKey]);

  useEffect(() => {
    const current = element.current;
    if (!current || !localSource) return;
    const stream = new MediaStream([nativeCaptureTrack(localSource, "video")]);
    current.srcObject = stream;
    let cancelled = false;
    const retry = async () => {
      if (!cancelled) await current.play();
    };
    if (autoPlay) {
      void retry().catch((error: unknown) => {
        if (!cancelled) callback.current?.({ error, retry });
      });
    }
    return () => {
      cancelled = true;
      if (current.srcObject === stream) current.srcObject = null;
    };
  }, [local, localSource, autoPlay]);

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
