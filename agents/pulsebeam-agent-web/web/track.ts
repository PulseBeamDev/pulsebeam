export type Delivery = "latest" | "lossy" | "ordered";

export interface VideoTrackSpec {
  readonly kind: "video";
  readonly label: string;
}

export interface AudioTrackSpec {
  readonly kind: "audio";
  readonly label: string;
}

export interface DataTrackSpec<D extends Delivery = Delivery> {
  readonly kind: "data";
  readonly label: string;
  readonly delivery: D;
}

export type TrackSpec = VideoTrackSpec | AudioTrackSpec | DataTrackSpec;

function label(value: string): string {
  if (typeof value !== "string")
    throw new TypeError("track label must be a string");
  return value;
}

export const track = Object.freeze({
  video(value: string): VideoTrackSpec {
    return Object.freeze({ kind: "video", label: label(value) });
  },
  audio(value: string): AudioTrackSpec {
    return Object.freeze({ kind: "audio", label: label(value) });
  },
  data<D extends Delivery>(value: string, delivery: D): DataTrackSpec<D> {
    if (delivery !== "latest" && delivery !== "lossy" && delivery !== "ordered")
      throw new TypeError("invalid data delivery mode");
    return Object.freeze({ kind: "data", label: label(value), delivery });
  },
});

export function trackKey(spec: TrackSpec): string {
  return JSON.stringify(
    spec.kind === "data"
      ? [spec.kind, spec.label, spec.delivery]
      : [spec.kind, spec.label],
  );
}
