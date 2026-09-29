import {
  AgentProvider,
  createAgent,
  useAgent,
  useDisplayMedia,
  useMediaDevices,
  useRemoteMedia,
  useUserMedia,
  Video,
  Audio,
  type AgentProviderProps,
  type Agent,
  type AgentConfig,
  type AgentEvent,
  type AgentFailure,
  type AgentState,
  type CapturedVideoTrack,
  type LocalTrackCapacityError,
  type MediaCaptureError,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteMediaAttachmentOptions,
  type UseAgentResult,
  type RemoteTrack,
  type RemoteVideoTrack,
  type RemoteAudioSource,
  type TopicMode,
  type VideoSenderConfig,
} from "@pulsebeam/react";
import type { ReactElement, ReactNode, RefObject } from "react";

declare const children: ReactNode;
declare const media: MediaStreamTrack;
declare const element: RefObject<HTMLMediaElement | null>;
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
const state: AgentState = {
  connected: true,
  publications: [
    { slot: "v0", label: "camera", active: true },
    { slot: "a0", label: "mic", active: true },
    { slot: "v1", label: "screen", active: true },
  ],
  video: [
    {
      slot: 0,
      trackId: "remote-camera",
      height: 720,
      minHeight: 360,
      minFps: 24,
      priority: 1,
      playoutDelay: { mode: "fixed", minMs: 50, maxMs: 100 },
    },
  ],
  audio: { automatic: true },
  topics: [
    { name: "chat", mode: "ordered", publish: true, subscribe: true },
    { name: "reaction", mode: "latest", publish: true, subscribe: true },
  ],
};

const props: AgentProviderProps = { agent, children };
const provider: ReactElement = AgentProvider(props);
const result: UseAgentResult = useAgent();
const owned: Agent | null = useAgent(config);
const camera = useUserMedia({ video: { width: 1280 }, audio: false });
const screen = useDisplayMedia({ video: true, audio: false });
const devices = useMediaDevices();
if (owned) {
  owned.connect();
  const handle = owned.localVideoTrack("camera");
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
const playbackOptions: RemoteMediaAttachmentOptions = {
  publicationIds: ["remote-camera"],
  onPlaybackBlocked: (failure: PlaybackFailure, retry) => {
    void failure;
    void retry();
  },
};
const playback = useRemoteMedia(agent, element, playbackOptions);
const available: readonly RemoteVideoTrack[] = agent.remoteVideoTracks;
const mainAudio: RemoteAudioSource = agent.remoteAudio;
const remoteView: ReactElement | null = Video({
  source: available[0] ?? null,
  mirror: true,
});
const localView: ReactElement | null = Video({
  source: agent.localVideoTrack("camera"),
});
const remoteAudio: ReactElement | null = Audio({ source: mainAudio });
// @ts-expect-error Audio only accepts a remote audio source
Audio({ source: available[0] });
const messages = agent
  .topic<{ text: string }>("chat", { mode: "reliable" })
  .subscribe();
void [remoteView, localView, remoteAudio, messages];
declare const attachment: RemoteMediaAttachment;

const connection = result.connection;
const participantId = result.participantId;
const tracks: Readonly<Record<string, RemoteTrack>> = result.tracks;
result.setState(state);
const sender: VideoSenderConfig = { contentHint: "motion" };
void result.replaceLocalTrack("v0", media, sender);
void result.setLocalMuted("a0", false);
result.reconnect();
const mode: TopicMode = "ordered";
result.sendTopic("chat", mode, new Uint8Array());
const unsubscribe = result.subscribeEvents((event: AgentEvent) => {
  if (event.type === "failure") {
    const failure: AgentFailure = event;
    void failure;
  }
});
unsubscribe();
void playback.retryPlayback();
void attachment.retryPlayback();
agent.close();

void provider;
void connection;
void participantId;
void tracks;
