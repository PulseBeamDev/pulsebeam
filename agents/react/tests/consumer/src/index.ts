import {
  createAgent,
  useRemoteMedia,
  type Agent,
  type AgentConfig,
  type AgentState,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteMediaAttachmentOptions,
} from "@pulsebeam/react";
import type { RefObject } from "react";

declare const element: RefObject<HTMLMediaElement | null>;
declare const attachment: RemoteMediaAttachment;

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  roomId: "standup",
  topology: { localVideo: ["camera"], localAudio: ["mic"] },
};
const agent: Agent = createAgent(config);
const state: AgentState = {
  connected: false,
  publications: [],
  video: [],
  audio: { automatic: true },
  playoutDelay: { mode: "fixed", minMs: 50, maxMs: 100 },
  topics: [],
};

agent.setState(state);
const playbackOptions: RemoteMediaAttachmentOptions = {
  publicationIds: ["remote-camera"],
  onPlaybackBlocked: (failure: PlaybackFailure, retry) => {
    void failure;
    void retry();
  },
};
void useRemoteMedia(agent, element, playbackOptions).retryPlayback();
void attachment.retryPlayback();
agent.close();
