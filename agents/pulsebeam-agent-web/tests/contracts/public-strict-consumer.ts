import {
  createAgent,
  track,
  type DataEvent,
  createCaptureSource,
  attachRemoteMedia,
  attachRemoteVideo,
  attachRemoteAudio,
  ResourceOwnershipError,
  LocalTrackCapacityError,
  type AgentError,
  type AgentEvent,
  type AgentFailure,
  type AgentSnapshot,
  type AgentState,
  type ConnectionState,
  type FailureClass,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteVideoTrack,
  type RemoteAudioTrack,
  type TopicEvent,
} from "../../web/index.js";

// @ts-expect-error Managed activation is internal, not public construction.
import { createManagedAgent } from "../../web/index.js";
// @ts-expect-error Managed lifecycle types are not public.
import type { ManagedAgent } from "../../web/index.js";
// @ts-expect-error There is one direct constructor spelling.
import { createWebAgent } from "../../web/index.js";

const binaryAgent = createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque",
});
const audioPlayback = attachRemoteAudio(binaryAgent, {
  onPlaybackError(failure) {
    const retry: () => Promise<void> = failure.retry;
    void retry;
  },
});
const playbackReady: Promise<void> = audioPlayback.ready;
const playbackClosed: Promise<void> = audioPlayback.close();
const ownershipError: Error = new ResourceOwnershipError("already owned");
void [playbackReady, playbackClosed, ownershipError];
const binarySpec = track.data("chat", "ordered");
const binary = binaryAgent.track(binarySpec);
const binarySend: Promise<void> = binary.send(new Uint8Array([0, 255]));
const binaryEvents: AsyncIterableIterator<DataEvent<"ordered">> = binary.events(
  { signal: new AbortController().signal },
);
const remoteBinary = binaryAgent.remote.participant("alice").track(binarySpec);
const remoteId: string | null = remoteBinary.id;
const remoteActive: boolean = remoteBinary.active;
const remoteEvents: AsyncIterableIterator<DataEvent<"ordered">> =
  remoteBinary.events();
void [binarySend, binaryEvents, remoteEvents, remoteId, remoteActive];
// @ts-expect-error Descriptors are immutable values.
binarySpec.label = "different";
// @ts-expect-error Data delivery is explicit.
track.data("chat");
// @ts-expect-error No string codec in the SDK.
binary.send("text");
// @ts-expect-error No JSON codec in the SDK.
binary.send({ text: "text" });
// @ts-expect-error Remote data handles are receive-only.
remoteBinary.send(new Uint8Array());
// @ts-expect-error Local data handles have no publication method.
binary.publish(new Uint8Array());
declare const lossyEvent: DataEvent<"lossy">;
// @ts-expect-error Only ordered data streams include gaps.
lossyEvent.from;
declare const binaryMessage: Extract<DataEvent, { type: "message" }>;
const binarySequence: bigint = binaryMessage.sequence;
const binaryPayload: Uint8Array = binaryMessage.data;
// @ts-expect-error Message identity is trackId, not the old topic identity API.
binaryMessage.dataTrackId;
void [binarySequence, binaryPayload];

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

const defaults = createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
});
const overridden = createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: { remoteVideos: 3 },
});
void [defaults, overridden];

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
  audio: {
    pinned: ["publication-audio"],
    selected: [{ participantId: "alice", label: "microphone" }],
    automatic: true,
  },
  topics: [
    { name: "presence", mode: "latest", publish: true, subscribe: true },
    { name: "chat", mode: "ordered", subscribe: true },
  ],
};
agent.setState(desired);

agent.connect();
const camera = agent.local.video("camera");
const microphone = agent.local.audio("camera");
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
const connection: ConnectionState = agent.connection;
const agentError: AgentError | null = agent.error;
void [connection, agentError];
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
  agent.remote.videoTracks[0];
const discoveredAudio: RemoteAudioTrack | undefined =
  agent.remote.audioTracks[0];
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
const roomChat = agent.topic<{ message: string }>("chat", { mode: "ordered" });
agent.topic("chat", { mode: "latest" });
agent.topic("chat", { mode: "lossy" });
void roomChat.publish({ message: "hello" });
const liveMessages: AsyncIterable<TopicEvent<{ message: string }>> =
  roomChat.subscribe();
void liveMessages;
// @ts-expect-error topic payload must match its declared type
void roomChat.publish({ message: 42 });
// @ts-expect-error removed transport-named aliases cannot select a mode
agent.topic("chat", { mode: "reliable" });
// @ts-expect-error unreliable is not a delivery policy
agent.topic("chat", { mode: "unreliable" });
// @ts-expect-error topic mode is mandatory
agent.topic("chat", {});
// @ts-expect-error subscriptions are topic-wide, not publisher-scoped
roomChat.subscribe({ publisherId: "alice" });
const participantId: string | null = agent.participantId;
const roomId: string | null = agent.roomId;
const catalogParticipantId: string =
  snapshot.catalog.participants[0].participantId;
void [participantId, roomId, catalogParticipantId];
// @ts-expect-error normal snapshots do not expose canonical participant IDs
snapshot.catalog.participants[0].id;
// @ts-expect-error removed identity alias
snapshot.participantExternalId;
// @ts-expect-error removed identity alias
snapshot.roomExternalId;
// @ts-expect-error removed Agent identity alias
agent.participantExternalId;
// @ts-expect-error removed Agent identity alias
agent.roomExternalId;
declare const topicEvent: TopicEvent<{ message: string }>;
if (topicEvent.type === "message") {
  const sequence: bigint = topicEvent.sequence;
  const track: string = topicEvent.dataTrackId;
  const sender: string = topicEvent.participantId;
  const value: string = topicEvent.value.message;
  void [sequence, track, sender, value];
} else {
  const expected: bigint = topicEvent.expectedSequence;
  const available: bigint = topicEvent.availableSequence;
  void [expected, available];
  // @ts-expect-error gaps do not carry a pretend application message
  topicEvent.value;
}
// @ts-expect-error no parallel publisher identity
topicEvent.publisherId;
// @ts-expect-error no parallel stream identity
topicEvent.streamId;
// @ts-expect-error no epoch identity
topicEvent.streamEpoch;
// @ts-expect-error continuity is an inline gap, not an application-frame flag
topicEvent.resyncRequired;
const replacement: Promise<void> = agent.replaceLocalTrack("a0", audioTrack, {
  contentHint: "speech",
  encodings: [],
});
const muted: Promise<void> = agent.setLocalMuted("a0", true);
const attachment: RemoteMediaAttachment = attachRemoteMedia(
  agent,
  videoElement,
  {
    publicationIds: ["publication-audio"],
    onPlaybackBlocked: (failure: PlaybackFailure, retry) => {
      void failure.message;
      void retry();
    },
  },
);
attachment.setPublicationIds(["publication-audio"]);
const participant = agent.remote.participant("alice");
const remoteParticipantId: string = participant.participantId;
void remoteParticipantId;
// @ts-expect-error removed remote identity alias
participant.externalId;
const voice = participant.audio("microphone");
const receiving: boolean = voice.receiving;
voice.readWaveform(new Float32Array(2048));
voice.readSpectrum(new Float32Array(1024));
voice.subscribe(() => {});
void agent.remote.resumeAudio();
void receiving;
// @ts-expect-error Native media is only available on the low-level snapshot.
voice.media;
// @ts-expect-error Audio level telemetry is not part of the logical API.
voice.level;
// @ts-expect-error Aggregate audio playback sources were removed.
agent.remoteAudio;
// @ts-expect-error Top-level discovery was removed.
agent.remoteAudioTracks;
// @ts-expect-error Old local lookup methods were removed.
agent.localVideoTrack("camera");
// @ts-expect-error Generic attachment cannot provide an alternate audio playback model.
attachRemoteMedia(agent, audioElement, { publicationIds: [] });

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
