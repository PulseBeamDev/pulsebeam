import {
  Audio,
  Data,
  Publish,
  Video,
  track,
  useAgent,
  useCapture,
  useDevices,
  type Agent,
  type AgentConfig,
  type CameraDevice,
  type MicrophoneDevice,
  type VideoCaptureSettings,
  type AudioCaptureSettings,
} from "@pulsebeam/react";
import { createAgent } from "@pulsebeam/web";
import { createElement } from "react";

// @ts-expect-error React owns Agents only through useAgent.
import { createAgent as createReactAgent } from "@pulsebeam/react";
// @ts-expect-error Managed lifecycle is not public Web API.
import { createManagedAgent } from "@pulsebeam/web";
// @ts-expect-error No provider hierarchy.
import { AgentProvider } from "@pulsebeam/react";
// @ts-expect-error Replaced capture hooks are absent.
import { useUserMedia } from "@pulsebeam/react";
// @ts-expect-error Replaced capture hooks are absent.
import { useDisplayMedia } from "@pulsebeam/react";
// @ts-expect-error Replaced device hook is absent.
import { useMediaDevices } from "@pulsebeam/react";

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
const agent: Agent = useAgent(config);
const direct = createAgent(config);
direct.connect();
direct.close();
// @ts-expect-error Required ownership configuration.
useAgent();

const devices = useDevices();
const cameraDevice: CameraDevice | undefined = devices.cameras[0];
const microphone: MicrophoneDevice | undefined = devices.microphones[0];
// @ts-expect-error Speaker inventory is outside capture.
devices.speakers;
const capture = useCapture("device", {
  video: {
    device: cameraDevice,
    width: 1280,
    height: 720,
    frameRate: 30,
  },
  audio: { device: microphone, echoCancellation: true },
});
useCapture("device", { video: true, audio: true });
useCapture("display", { video: { height: 1080 }, audio: true });
// @ts-expect-error Browser constraints are not portable options.
useCapture("device", { video: { width: { exact: 1280 } }, audio: false });
// @ts-expect-error Device kinds are distinct.
useCapture("device", { video: { device: microphone }, audio: false });
// @ts-expect-error Display capture has no exact device selection.
useCapture("display", { video: { device: cameraDevice }, audio: false });
// @ts-expect-error Display video is required.
useCapture("display", { video: false, audio: false });

const localVideo = agent.track(track.video("camera"));
const localAudio = agent.track(track.audio("microphone"));
const chat = agent.track(track.data("chat", "ordered"));
createElement(Publish, {
  track: localVideo,
  source: capture.video,
  enabled: false,
  content: "detail",
});
createElement(Publish, {
  track: localAudio,
  source: capture.audio,
  content: "music",
});
// @ts-expect-error Audio source cannot bind a video publication.
Publish({ track: localVideo, source: capture.audio });
// @ts-expect-error Publication content is kind-specific.
Publish({ track: localVideo, source: capture.video, content: "speech" });
// @ts-expect-error No normal imperative publication setter.
localVideo.setSource(capture.video);

const participant = agent.remote.participant("alice");
const remoteVideo = participant.track(track.video("camera"));
const remoteAudio = participant.track(track.audio("microphone"));
const remoteChat = participant.track(track.data("chat", "ordered"));
createElement(Video, {
  source: remoteVideo,
  receive: {
    minHeight: 360,
    minFps: 15,
    priority: 1,
    playoutDelay: { minMs: 0, maxMs: 100 },
  },
});
createElement(Video, { source: capture.video });
createElement(Audio, {
  agent,
  onPlaybackError: (failure) => void failure.retry(),
});
createElement(Data, {
  track: remoteChat,
  onEvent(event) {
    const participantId: string = event.participantId;
    const trackId: string = event.trackId;
    if (event.type === "message") {
      const bytes: Uint8Array = event.data;
      const sequence: bigint = event.sequence;
      void [bytes, sequence];
    } else {
      const from: bigint = event.from;
      const to: bigint = event.to;
      void [from, to];
    }
    void [participantId, trackId];
  },
});
createElement(Data, { track: chat, onEvent: () => {} });
void chat.send(new Uint8Array([0, 255]));
// @ts-expect-error React data handles own receive only through Data.
chat.events();
// @ts-expect-error React remote data handles own receive only through Data.
remoteChat.events();
// @ts-expect-error Remote data is receive-only.
remoteChat.send(new Uint8Array());
// @ts-expect-error Remote receive policy belongs to Video.
remoteVideo.setReceiveOptions({});
// @ts-expect-error Audio analysis is not per-track React API.
remoteAudio.readWaveform(new Float32Array());
// @ts-expect-error Kind-specific remote collection roots were removed.
agent.remote.videoTracks;
// @ts-expect-error Kind-specific participant collection roots were removed.
participant.audioTracks;
// @ts-expect-error Old callable local roots were removed.
agent.local;
// @ts-expect-error Typed JSON topics were removed.
agent.topic("chat", { mode: "ordered" });
// @ts-expect-error Retry is connect(), not a second session operation.
agent.reconnect();
// @ts-expect-error React owns release.
agent.close();
// @ts-expect-error No low-level normal observation.
agent.getSnapshot();

const videoSettings: VideoCaptureSettings | undefined = capture.video?.settings;
const audioSettings: AudioCaptureSettings | undefined = capture.audio?.settings;
void [videoSettings, audioSettings, remoteAudio, participant.tracks];
agent.connect();
agent.disconnect();
