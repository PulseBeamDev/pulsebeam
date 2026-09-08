import {
  createAgent,
  type AgentEvent,
  type AgentFailure,
  type AgentSnapshot,
  type AgentState,
  type FailureClass,
} from "../../web/index.js";

declare const audioTrack: MediaStreamTrack;

const agent = createAgent({
  endpoint: "https://pulsebeam.example",
  roomId: "meet",
  requestHeaders: { "x-session": "example" },
  topology: {
    localAudio: ["microphone"],
    localVideo: ["camera", "screen"],
    remoteAudio: 3,
    remoteVideo: 7,
  },
});

const desired: AgentState = {
  connected: true,
  publications: [{ slot: "microphone", active: true }],
  video: [
    {
      slot: 0,
      trackId: "publication-video",
      height: 720,
      minHeight: 180,
      minFps: 15,
      priority: 100,
    },
  ],
  audio: { pinned: ["publication-audio"], automatic: true },
  playoutDelay: { mode: "fixed", minMs: 100, maxMs: 250 },
  topics: [
    { name: "presence", mode: "latest", publish: true, subscribe: true },
    { name: "chat", mode: "ordered", subscribe: true },
  ],
};
agent.setState(desired);

const snapshot: AgentSnapshot = agent.getSnapshot();
if (snapshot.failure) {
  const failure: AgentFailure = snapshot.failure;
  const failureClass: FailureClass = failure.class;
  const failureMessage: string = failure.message;
  void failureClass;
  void failureMessage;
}
const media: MediaStreamTrack | undefined =
  snapshot.tracks["publication-audio"]?.media;
const removeSnapshot = agent.subscribe(() => agent.getSnapshot());
const removeEvents = agent.subscribeEvents((event: AgentEvent) => {
  if (event.type === "topic-message") {
    const payload: Uint8Array = event.payload;
    void payload;
  }
  if (event.type === "failure") {
    const failureClass: FailureClass = event.class;
    const failureMessage: string = event.message;
    void failureClass;
    void failureMessage;
  }
});
const replacement: Promise<void> = agent.replaceLocalTrack(
  "microphone",
  audioTrack,
  { contentHint: "speech", encodings: [] },
);
const muted: Promise<void> = agent.setLocalMuted("microphone", true);
agent.sendTopic("presence", "latest", new Uint8Array([1]));
agent.sendTopic("chat", "ordered", new Uint8Array([2]));
agent.reconnect();
removeSnapshot();
removeEvents();

void media;
void replacement;
void muted;
