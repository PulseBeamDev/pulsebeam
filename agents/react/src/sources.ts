import type {
  CapturedAudioTrack,
  CapturedVideoTrack,
  ReceiveOptions,
} from "@pulsebeam/web";
import { nativeCaptureTrack } from "@pulsebeam/web/internal/react";

const sourceBrand = Symbol("PulseBeam source");
const publishableBrand = Symbol("PulseBeam publishable source");

export interface VideoSource {
  readonly kind: "video";
  readonly [sourceBrand]: true;
}
export interface AudioSource {
  readonly kind: "audio";
  readonly [sourceBrand]: true;
}
export interface PublishableVideoSource extends VideoSource {
  readonly [publishableBrand]: true;
}
export interface PublishableAudioSource extends AudioSource {
  readonly [publishableBrand]: true;
}

export interface VideoAttachment {
  setAutoPlay(autoPlay: boolean): void;
  setPreferences(preferences: ReceiveOptions): void;
  close(): void;
}
type PlaybackError = (error: unknown, retry: () => Promise<void>) => void;
export type VideoBinding = (
  element: HTMLVideoElement,
  onError: PlaybackError,
  options: { autoPlay: boolean; preferences: ReceiveOptions },
) => VideoAttachment;

const videos = new WeakMap<VideoSource, VideoBinding>();
const tracks = new WeakMap<object, CapturedVideoTrack | CapturedAudioTrack>();

export function videoSource<T extends { readonly kind: "video" }>(
  value: T,
  binding: VideoBinding,
): T & VideoSource {
  const source = Object.defineProperty(value, sourceBrand, {
    value: true,
  }) as T & VideoSource;
  videos.set(source, binding);
  return source;
}
export function audioSource<T extends { readonly kind: "audio" }>(
  value: T,
): T & AudioSource {
  return Object.defineProperty(value, sourceBrand, { value: true }) as T &
    AudioSource;
}
export function attachVideoSource(
  source: VideoSource,
  ...args: Parameters<VideoBinding>
): VideoAttachment {
  const bind = videos.get(source);
  if (!bind) throw new TypeError("invalid video source");
  return bind(...args);
}

export function publishableVideoSource<T extends { readonly kind: "video" }>(
  value: T,
  track: CapturedVideoTrack,
): T & PublishableVideoSource {
  const source = videoSource(
    value,
    localVideoBinding(() => track),
  );
  tracks.set(source, track);
  return Object.defineProperty(source, publishableBrand, { value: true }) as T &
    PublishableVideoSource;
}
export function publishableAudioSource<T extends { readonly kind: "audio" }>(
  value: T,
  track: CapturedAudioTrack,
): T & PublishableAudioSource {
  const source = audioSource(value);
  tracks.set(source, track);
  return Object.defineProperty(source, publishableBrand, { value: true }) as T &
    PublishableAudioSource;
}
export function publicationTrack(
  source: PublishableVideoSource | PublishableAudioSource,
  kind: "video" | "audio",
) {
  const track = tracks.get(source);
  if (!track || source.kind !== kind)
    throw new TypeError(`invalid publishable ${kind} source`);
  return track;
}

export function localVideoBinding(
  read: () => CapturedVideoTrack | null,
  subscribe: (listener: () => void) => () => void = () => () => {},
): VideoBinding {
  return (element, onError, options) => {
    let autoPlay = options.autoPlay;
    let live = true;
    let current: CapturedVideoTrack | null | undefined;
    let stream: MediaStream | null = null;
    const play = () => {
      const attached = stream;
      const retry = async () => {
        if (live && attached && stream === attached) await element.play();
      };
      void retry().catch((error: unknown) => {
        if (live && stream === attached && autoPlay) onError(error, retry);
      });
    };
    const sync = () => {
      const next = read();
      if (next === current) return;
      current = next;
      if (stream && element.srcObject === stream) element.srcObject = null;
      const track = next ? nativeCaptureTrack(next, "video") : null;
      stream = track?.readyState === "live" ? new MediaStream([track]) : null;
      if (!stream) return;
      element.srcObject = stream;
      if (autoPlay) play();
    };
    const unsubscribe = subscribe(sync);
    sync();
    return {
      setAutoPlay(value) {
        if (!live || value === autoPlay) return;
        autoPlay = value;
        if (autoPlay && stream) play();
      },
      setPreferences() {},
      close() {
        live = false;
        unsubscribe();
        if (stream && element.srcObject === stream) element.srcObject = null;
        stream = null;
      },
    };
  };
}
