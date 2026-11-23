import {
  AudioSource as GeneratedAudioSource,
  VideoSource as GeneratedVideoSource,
  type AudioSource as WebAudioSource,
  type VideoSource as WebVideoSource,
} from "@pulsebeam/web";

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
const tracks = new WeakMap<object, WebVideoSource | WebAudioSource>();

function source<T extends { readonly kind: "video" | "audio" }>(
  value: T,
  track: WebVideoSource | WebAudioSource,
) {
  tracks.set(value, track);
  Object.defineProperty(value, sourceBrand, { value: true });
  return Object.defineProperty(value, publishableBrand, { value: true });
}
export function publishableVideoSource<T extends { readonly kind: "video" }>(
  value: T,
  track: WebVideoSource,
): T & PublishableVideoSource {
  return source(value, track) as T & PublishableVideoSource;
}
export function publishableAudioSource<T extends { readonly kind: "audio" }>(
  value: T,
  track: WebAudioSource,
): T & PublishableAudioSource {
  return source(value, track) as T & PublishableAudioSource;
}
export function publicationTrack(
  value: PublishableVideoSource | PublishableAudioSource,
  kind: "video" | "audio",
) {
  const track = tracks.get(value);
  if (!track || value.kind !== kind)
    throw new TypeError(`invalid publishable ${kind} source`);
  return track;
}
export function playbackTrack(
  value: VideoSource | AudioSource | WebVideoSource | WebAudioSource,
  kind: "video" | "audio",
) {
  const track =
    tracks.get(value) ?? ("previewTrack" in value ? value : undefined);
  if (
    kind === "video"
      ? !(track instanceof GeneratedVideoSource)
      : !(track instanceof GeneratedAudioSource)
  )
    throw new TypeError(`invalid ${kind} source`);
  return track!;
}
export function releasePublicationSource(
  value: PublishableVideoSource | PublishableAudioSource | null,
): void {
  if (value)
    void tracks
      .get(value)
      ?.close()
      .catch((error: unknown) => {
        console.error("PulseBeam capture source cleanup failed", error);
      });
}
