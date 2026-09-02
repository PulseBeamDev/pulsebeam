import type { AudioPolicy, MediaKind, VideoPolicy } from "./types.js";

export interface SenderEncodingConfig {
  rid?: string;
  active: boolean;
  scaleResolutionDownBy?: number;
  maxBitrate?: number;
  maxFramerate?: number;
  scalabilityMode?: string;
  dtx?: "enabled" | "disabled";
}

export interface SenderConfig {
  contentHint: string;
  degradationPreference?: RTCDegradationPreference;
  encodings: SenderEncodingConfig[];
}

const VIDEO_LAYERS = [
  { rid: "q", scale: 4, weight: 0.15 },
  { rid: "h", scale: 2, weight: 0.35 },
  { rid: "f", scale: 1, weight: 1 },
] as const;

export function videoSenderConfig(policy: VideoPolicy): SenderConfig {
  const detail = policy === "detail";
  const baseBitrate = detail ? 2_500_000 : 1_250_000;
  const maxFramerate = detail ? 15 : 30;
  return {
    contentHint: policy,
    degradationPreference: detail ? "maintain-resolution" : "maintain-framerate",
    encodings: VIDEO_LAYERS.map((layer, index) => ({
      rid: layer.rid,
      active: !detail || index === VIDEO_LAYERS.length - 1,
      scaleResolutionDownBy: layer.scale,
      maxBitrate: Math.floor(baseBitrate * layer.weight),
      maxFramerate: detail ? [1, 8, maxFramerate][index] : maxFramerate,
      scalabilityMode: detail ? "L1T2" : "L1T3",
    })),
  };
}

export function audioSenderConfig(policy: AudioPolicy): SenderConfig {
  return policy === "music"
    ? {
        contentHint: "music",
        encodings: [{ active: true, maxBitrate: 128_000, dtx: "disabled" }],
      }
    : {
        contentHint: "speech",
        encodings: [{ active: true, maxBitrate: 48_000, dtx: "enabled" }],
      };
}

export function senderConfig(kind: MediaKind, policy: VideoPolicy | AudioPolicy): SenderConfig {
  return kind === "video"
    ? videoSenderConfig(policy as VideoPolicy)
    : audioSenderConfig(policy as AudioPolicy);
}
