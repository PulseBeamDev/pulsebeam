const nativeTracks = new WeakMap<object, MediaStreamTrack>();

declare const captureSourceBrand: unique symbol;

export interface CapturedVideoTrack {
  readonly kind: "video";
  readonly [captureSourceBrand]: true;
}

export interface CapturedAudioTrack {
  readonly kind: "audio";
  readonly [captureSourceBrand]: true;
}

export type CapturedTrack = CapturedVideoTrack | CapturedAudioTrack;

export function createCaptureSource(
  track: MediaStreamTrack,
  kind: "video",
): CapturedVideoTrack;
export function createCaptureSource(
  track: MediaStreamTrack,
  kind: "audio",
): CapturedAudioTrack;
export function createCaptureSource(
  track: MediaStreamTrack,
  kind: "video" | "audio",
): CapturedTrack {
  if (track.kind !== kind) throw new TypeError("capture track kind mismatch");
  const source = Object.freeze({ kind });
  nativeTracks.set(source, track);
  return source as CapturedTrack;
}

export function nativeCaptureTrack(
  source: CapturedTrack,
  kind: "video" | "audio",
): MediaStreamTrack {
  const track = nativeTracks.get(source);
  if (!track || source.kind !== kind || track.kind !== kind) {
    throw new TypeError("invalid capture source");
  }
  return track;
}
