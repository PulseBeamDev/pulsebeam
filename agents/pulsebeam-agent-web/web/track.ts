export type Delivery = "latest" | "lossy" | "ordered";

export interface VideoTrackSpec {
  readonly kind: "video";
  readonly trackId: string;
}

export interface AudioTrackSpec {
  readonly kind: "audio";
  readonly trackId: string;
}

export interface DataTrackSpec<D extends Delivery = Delivery> {
  readonly kind: "data";
  readonly trackId: string;
  readonly delivery: D;
}

export type TrackSpec = VideoTrackSpec | AudioTrackSpec | DataTrackSpec;

function trackName(value: string): string {
  if (typeof value !== "string")
    throw new TypeError("trackId must be a string");
  return value;
}

export const track = Object.freeze({
  video(value: string): VideoTrackSpec {
    return Object.freeze({ kind: "video", trackId: trackName(value) });
  },
  audio(value: string): AudioTrackSpec {
    return Object.freeze({ kind: "audio", trackId: trackName(value) });
  },
  data<D extends Delivery>(value: string, delivery: D): DataTrackSpec<D> {
    if (delivery !== "latest" && delivery !== "lossy" && delivery !== "ordered")
      throw new TypeError("invalid data delivery mode");
    return Object.freeze({ kind: "data", trackId: trackName(value), delivery });
  },
});

export function trackKey(spec: TrackSpec): string {
  return JSON.stringify(
    spec.kind === "data"
      ? [spec.kind, spec.trackId, spec.delivery]
      : [spec.kind, spec.trackId],
  );
}
