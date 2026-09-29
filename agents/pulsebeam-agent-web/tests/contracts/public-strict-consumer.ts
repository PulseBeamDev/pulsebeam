import {
  createAgent,
  createCaptureSource,
  attachRemoteMedia,
  attachRemoteAudio,
  attachRemoteVideo,
  LocalTrackCapacityError,
  type AgentEvent,
  type AgentFailure,
  type AgentSnapshot,
  type AgentState,
  type FailureClass,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteVideoTrack,
  type RemoteAudioTrack,
} from "../../web/index.js";

declare const audioTrack: MediaStreamTrack;
declare const audioElement: HTMLAudioElement;
const agent = createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: {
    localAudios: 1,
    localVideos: 2,
    remoteAudios: 3,
    remoteVideos: 7,
  },
  logging: { level: "debug" },
});

declare const legacyConfig: import("../../web/index.js").AgentConfig;
// @ts-expect-error room identity is owned by the bearer token
legacyConfig.roomId;
// @ts-expect-error arbitrary request headers are not part of the public API
legacyConfig.requestHeaders;

const desired: AgentState = {
  connected: true,
  publications: [{ slot: "a0", label: "microphone", active: true }],
  video: [
    {
      slot: 0,
      trackId: "publication-video",
      height: 720,
      minHeight: 180,
      minFps: 15,
      priority: 100,
      playoutDelay: { mode: "fixed", minMs: 100, maxMs: 250 },
    },
  ],
  audio: { pinned: ["publication-audio"], automatic: true },
  topics: [
    { name: "presence", mode: "latest", publish: true, subscribe: true },
    { name: "chat", mode: "ordered", subscribe: true },
  ],
};
agent.setState(desired);

agent.connect();
const camera = agent.localVideoTrack("camera");
const microphone = agent.localAudioTrack("camera");
const videoSource = createCaptureSource(audioTrack, "video");
const audioSource = createCaptureSource(audioTrack, "audio");
camera.setSource(videoSource);
microphone.setSource(audioSource);
// @ts-expect-error Audio sources cannot attach to video handles.
camera.setSource(audioSource);
// @ts-expect-error Logical handle sources are not raw browser tracks.
camera.setSource(audioTrack);
const videoCapacity: LocalTrackCapacityError = new LocalTrackCapacityError(
  "video",
  "screen",
  1,
);
void videoCapacity;
agent.disconnect();
const snapshot: AgentSnapshot = agent.getSnapshot();
const catalogRevision: number = snapshot.catalog.revision;
const acceptedIntentRevision: number = snapshot.mapping.acceptedIntentRevision;
const mappedPublication: string | undefined =
  snapshot.mapping.audio[0]?.publicationId;
void catalogRevision;
void acceptedIntentRevision;
void mappedPublication;
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
const discoveredVideo: RemoteVideoTrack | undefined =
  agent.remoteVideoTracks[0];
const discoveredAudio: RemoteAudioTrack | undefined =
  agent.remoteAudioTracks[0];
if (discoveredVideo) {
  const participant: string = discoveredVideo.participantId;
  const label: string = discoveredVideo.label;
  discoveredVideo.setReceiveOptions({
    minHeight: 360,
    minFps: 24,
    priority: 2,
  });
  const unsubscribeVideo = discoveredVideo.subscribe(
    () => discoveredVideo.active,
  );
  unsubscribeVideo();
  void [participant, label];
}
if (discoveredAudio) void discoveredAudio.label;
const roomChat = agent.topic<{ message: string }>("chat", { mode: "reliable" });
void roomChat.publish({ message: "hello" });
const liveMessages: AsyncIterable<{ message: string }> = roomChat.subscribe();
void liveMessages;
// @ts-expect-error topic payload must match its declared type
void roomChat.publish({ message: 42 });
// @ts-expect-error public topic API selects reliable or unreliable, not core wire mode
agent.topic("chat", { mode: "ordered" });
const replacement: Promise<void> = agent.replaceLocalTrack("a0", audioTrack, {
  contentHint: "speech",
  encodings: [],
});
const muted: Promise<void> = agent.setLocalMuted("a0", true);
const attachment: RemoteMediaAttachment = attachRemoteMedia(
  agent,
  audioElement,
  {
    publicationIds: ["publication-audio"],
    onPlaybackBlocked: (failure: PlaybackFailure, retry) => {
      void failure.message;
      void retry();
    },
  },
);
attachment.setPublicationIds(["publication-audio"]);
const audioPlayback = attachRemoteAudio(agent.remoteAudio, audioElement);
audioPlayback.close();
declare const videoElement: HTMLVideoElement;
if (discoveredVideo) attachRemoteVideo(discoveredVideo, videoElement).close();
if (discoveredVideo) {
  // @ts-expect-error video source cannot be attached to an audio element
  attachRemoteVideo(discoveredVideo, audioElement);
}
void attachment.retryPlayback();
attachment.close();
agent.sendTopic("presence", "latest", new Uint8Array([1]));
agent.sendTopic("chat", "ordered", new Uint8Array([2]));
agent.reconnect();
removeSnapshot();
removeEvents();

void media;
void replacement;
void muted;
