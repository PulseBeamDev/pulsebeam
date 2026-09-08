import {
  createAgent,
  type Agent,
  type AgentConfig,
  type AgentState,
} from "@pulsebeam/react";

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
agent.close();
