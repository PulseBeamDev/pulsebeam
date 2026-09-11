import {
  AgentProvider,
  createAgent,
  useAgent,
  useRemoteMedia,
  type AgentProviderProps,
  type Agent,
  type AgentConfig,
  type AgentEvent,
  type AgentFailure,
  type AgentState,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteMediaAttachmentOptions,
  type UseAgentResult,
  type RemoteTrack,
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
    localVideo: ["camera", "screen"],
    localAudio: ["mic"],
    remoteVideo: 9,
    remoteAudio: 9,
  },
  logging: { level: "info" },
};
const agent: Agent = createAgent(config);
const state: AgentState = {
  connected: true,
  publications: [
    { slot: "camera", active: true },
    { slot: "mic", active: true },
    { slot: "screen", active: true },
  ],
  video: [
    {
      slot: 0,
      trackId: "remote-camera",
      height: 720,
      minHeight: 360,
      minFps: 24,
      priority: 1,
    },
  ],
  audio: { automatic: true },
  playoutDelay: { mode: "fixed", minMs: 50, maxMs: 100 },
  topics: [
    { name: "chat", mode: "ordered", publish: true, subscribe: true },
    { name: "reaction", mode: "latest", publish: true, subscribe: true },
  ],
};

const props: AgentProviderProps = { agent, children };
const provider: ReactElement = AgentProvider(props);
const result: UseAgentResult = useAgent();
const playbackOptions: RemoteMediaAttachmentOptions = {
  publicationIds: ["remote-camera"],
  onPlaybackBlocked: (failure: PlaybackFailure, retry) => {
    void failure;
    void retry();
  },
};
const playback = useRemoteMedia(agent, element, playbackOptions);
declare const attachment: RemoteMediaAttachment;

const connection = result.connection;
const participantId = result.participantId;
const tracks: Readonly<Record<string, RemoteTrack>> = result.tracks;
result.setState(state);
const sender: VideoSenderConfig = { contentHint: "motion" };
void result.replaceLocalTrack("camera", media, sender);
void result.setLocalMuted("mic", false);
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
