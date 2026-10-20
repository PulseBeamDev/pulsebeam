import {
  useAgent,
  useDisplayMedia,
  useMediaDevices,
  useUserMedia,
  Video,
  type Agent,
  type AgentConfig,
  type CameraDevice,
  type MicrophoneDevice,
  type SpeakerDevice,
  type VideoSource,
  type AudioSource,
  type PublishableVideoSource,
  type PublishableAudioSource,
  type VideoCaptureSettings,
  type AudioCaptureSettings,
} from "@pulsebeam/react";
import { createAgent } from "@pulsebeam/web";
import { createElement } from "react";

// @ts-expect-error React owns Agents only through useAgent.
import { createAgent as createReactAgent } from "@pulsebeam/react";
// @ts-expect-error Managed lifecycle is not public Web API.
import { createManagedAgent } from "@pulsebeam/web";
// @ts-expect-error Managed lifecycle types are not public Web API.
import type { ManagedAgent } from "@pulsebeam/web";
// @ts-expect-error No provider hierarchy.
import { AgentProvider } from "@pulsebeam/react";
// @ts-expect-error No alternate Web construction spelling.
import { createWebAgent } from "@pulsebeam/web";

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
const owned: Agent = useAgent(config);
const direct = createAgent(config);
direct.setState({ connected: true });
direct.close();
// @ts-expect-error Required ownership configuration.
useAgent();
const devices = useMediaDevices();
const cameraDevice: CameraDevice | undefined = devices.cameras[0];
const microphone: MicrophoneDevice | undefined = devices.microphones[0];
const speaker: SpeakerDevice | undefined = devices.speakers[0];
const camera = useUserMedia({
  video: {
    device: cameraDevice,
    preferences: { width: 1280, height: 720, frameRate: 30 },
  },
  audio: { device: microphone, preferences: { echoCancellation: true } },
});
useUserMedia({ video: true, audio: true });
useDisplayMedia({ video: { preferences: { height: 1080 } }, audio: true });
// @ts-expect-error Browser constraints are not portable options.
useUserMedia({ video: { width: { exact: 1280 } }, audio: false });
// @ts-expect-error Device kinds are distinct.
useUserMedia({ video: { device: microphone }, audio: false });
// @ts-expect-error Speakers are inventory, not capture devices.
useUserMedia({ video: false, audio: { device: speaker } });
// @ts-expect-error Display chooser, not exact device selection.
useDisplayMedia({ video: { device: cameraDevice }, audio: false });
const Camera = owned.local.video("camera");
const Microphone = owned.local.audio("microphone");
createElement(Camera, {
  source: camera.video,
  enabled: false,
  preferences: { content: "detail", degradation: "balanced" },
});
createElement(Microphone, {
  source: camera.audio,
  preferences: { content: "music" },
});
// @ts-expect-error Audio source cannot bind video publication.
createElement(Camera, { source: camera.audio });
// @ts-expect-error No normal imperative publication setter.
Camera.setSource(camera.video);
// @ts-expect-error No competing normal desired-state API.
owned.setState({ connected: true });
// @ts-expect-error React owns release.
owned.close();
// @ts-expect-error No low-level normal observation.
owned.getSnapshot();
// @ts-expect-error No advanced API.
owned.advanced;
const Chat = owned.topic<{ text: string }>("chat", { mode: "reliable" });
createElement(Chat, {
  onMessage: (value, { publisherId }) => {
    const text: string = value.text;
    void [text, publisherId];
  },
  onError: (error) => {
    const code: "decode-failed" | "channel-failed" = error.code;
    void code;
  },
  onGap: (gap) => {
    const sequence: number = gap.expectedSequence;
    void sequence;
  },
});
void Chat.publish({ text: "hello" });
// @ts-expect-error No application subscription loop.
Chat.subscribe();
const remote = owned.remote.participant("alice").video("camera");
// @ts-expect-error Presentation owns receive preferences.
remote.setReceiveOptions({});
Video({
  source: remote,
  preferences: {
    minHeight: 360,
    minFps: 15,
    priority: 1,
    latency: { minMs: 0, maxMs: 100 },
  },
});
Video({ source: Camera });
Video({ source: camera.video });
owned.remote
  .participant("alice")
  .audio("microphone")
  .readWaveform(new Float32Array(2048));
const videoSources: readonly (VideoSource | null)[] = [
  camera.video,
  Camera,
  remote,
];
const audioSources: readonly (AudioSource | null)[] = [
  camera.audio,
  Microphone,
  owned.remote.participant("alice").audio("microphone"),
];
const publishableVideo: PublishableVideoSource | null = camera.video;
const publishableAudio: PublishableAudioSource | null = camera.audio;
const videoSettings: VideoCaptureSettings | undefined = camera.video?.settings;
const audioSettings: AudioCaptureSettings | undefined = camera.audio?.settings;
for (const source of videoSources) createElement(Video, { source });
// @ts-expect-error Remote playback is not remote forwarding.
createElement(Camera, { source: remote });
// @ts-expect-error Publication handles are not publishable inputs.
createElement(Camera, { source: Camera });
createElement(Microphone, {
  // @ts-expect-error Remote audio is not a publishable input.
  source: owned.remote.participant("alice").audio("microphone"),
});
// @ts-expect-error Audio sources cannot be rendered as video.
createElement(Video, { source: camera.audio });
// @ts-expect-error Kind alone is not a valid source capability.
createElement(Video, { source: { kind: "video" } });
// @ts-expect-error Capture no longer splits track from metadata.
camera.videoTrack;
// @ts-expect-error Capture no longer splits track from metadata.
camera.audioTrack;
// @ts-expect-error Capture sources do not expose their underlying tracks.
camera.video?.track;
void [
  audioSources,
  publishableVideo,
  publishableAudio,
  videoSettings,
  audioSettings,
];
const actualWidth: number | undefined = camera.video?.settings.width;
void [
  camera.video?.device,
  actualWidth,
  camera.audio?.settings.echoCancellation,
];
owned.connect();
owned.disconnect();
owned.reconnect();
