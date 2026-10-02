import {
  createAgent,
  useAgent,
  useDisplayMedia,
  useMediaDevices,
  useUserMedia,
  Video,
  type Agent,
  type AgentConfig,
  type CapturedVideoTrack,
  type LocalTrackCapacityError,
  type MediaCaptureError,
  type RemoteVideoTrack,
  type RemoteAudioTrack,
} from "@pulsebeam/react";
import type { ReactElement } from "react";

// @ts-expect-error Provider-based ownership was removed.
import { AgentProvider } from "@pulsebeam/react";
// @ts-expect-error Raw attachment hooks were replaced by source components.
import { useRemoteMedia } from "@pulsebeam/react";
// @ts-expect-error Provider snapshot adapters were removed.
import type { UseAgentResult } from "@pulsebeam/react";
// @ts-expect-error Raw attachment hook results were removed.
import type { UseRemoteMediaResult } from "@pulsebeam/react";

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: {
    localVideos: 2,
    localAudios: 1,
    remoteVideos: 9,
    remoteAudios: 9,
  },
  logging: { level: "info" },
};
const agent: Agent = createAgent(config);
const owned: Agent | null = useAgent(config);
// @ts-expect-error Each useAgent must have its own construction config.
useAgent();
const camera = useUserMedia({ video: { width: 1280 }, audio: false });
const screen = useDisplayMedia({ video: true, audio: false });
const devices = useMediaDevices();
if (owned) {
  owned.connect();
  const handle = owned.local.video("camera");
  handle.setSource(camera.videoTrack);
  const source: CapturedVideoTrack | null = handle.source;
  // @ts-expect-error An audio capture cannot be bound to a video handle.
  handle.setSource(camera.audioTrack);
  owned.disconnect();
  void source;
}
void screen.request();
const firstCameraId: string | undefined = devices.cameras[0]?.id;
declare const captureFailure: MediaCaptureError;
declare const exhausted: LocalTrackCapacityError;
const failureCode: string = captureFailure.code;
const capacity: number = exhausted.capacity;
void [firstCameraId, failureCode, capacity];
const available: readonly RemoteVideoTrack[] = agent.remote.videoTracks;
const mainAudio: RemoteAudioTrack = agent.remote
  .participant("alice")
  .audio("microphone");
const receiving: boolean = mainAudio.receiving;
mainAudio.readWaveform(new Float32Array(2048));
mainAudio.readSpectrum(new Float32Array(1024));
mainAudio.subscribe(() => {});
const remoteView: ReactElement | null = Video({
  source: available[0] ?? null,
  mirror: true,
});
const localView: ReactElement | null = Video({
  source: agent.local.video("camera"),
});
// @ts-expect-error Audio playback components were removed.
import { Audio } from "@pulsebeam/react";
// @ts-expect-error Aggregate playback sources were removed.
agent.remoteAudio;
const messages = agent
  .topic<{ text: string }>("chat", { mode: "reliable" })
  .subscribe();
void [remoteView, localView, receiving, messages];
agent.close();
