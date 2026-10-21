import {
  attachLocalTrack,
  attachRemoteAudio,
  attachRemoteVideo,
  createAgent,
  createCaptureSource,
  track,
  ResourceOwnershipError,
  type AgentError,
  type AgentSnapshot,
  type DataEvent,
  type PlaybackFailure,
  type RemoteAudioTrack,
  type RemoteVideoTrack,
} from "../../web/index.js";

// @ts-expect-error Managed activation is internal, not public construction.
import { createManagedAgent } from "../../web/index.js";
// @ts-expect-error Managed lifecycle types are not public.
import type { ManagedAgent } from "../../web/index.js";
// @ts-expect-error There is one direct constructor spelling.
import { createWebAgent } from "../../web/index.js";
// @ts-expect-error Raw media attachment is not a competing ownership path.
import { attachRemoteMedia } from "../../web/index.js";
// @ts-expect-error The JSON topic facade was removed.
import type { Topic } from "../../web/index.js";
// @ts-expect-error Desired-state mutation is not a normal public API.
import type { AgentState } from "../../web/index.js";

const agent = createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque",
});
agent.connect();
agent.disconnect();
agent.renewAuthorization("renewed");
agent.setLogging("info");
const snapshot: AgentSnapshot = agent.getSnapshot();
const error: AgentError | null = agent.error;
const unsubscribe = agent.subscribe(() => {});
void [snapshot, error, unsubscribe];
// @ts-expect-error Binary streams replace raw topic event observation.
agent.subscribeEvents(() => {});

const binarySpec = track.data("chat", "ordered");
const binary = agent.track(binarySpec);
const binarySend: Promise<void> = binary.send(new Uint8Array([0, 255]));
const binaryEvents: AsyncIterableIterator<DataEvent<"ordered">> = binary.events(
  { signal: new AbortController().signal },
);
const participant = agent.remote.participant("alice");
const remoteBinary = participant.track(binarySpec);
const remoteEvents: AsyncIterableIterator<DataEvent<"ordered">> =
  remoteBinary.events();
void [binarySend, binaryEvents, remoteEvents];
// @ts-expect-error Descriptors are immutable values.
binarySpec.label = "different";
// @ts-expect-error Data delivery is explicit.
track.data("chat");
// @ts-expect-error No string codec in the SDK.
binary.send("text");
// @ts-expect-error Remote data handles are receive-only.
remoteBinary.send(new Uint8Array());
declare const orderedEvent: DataEvent<"ordered">;
if (orderedEvent.type === "message") {
  const sequence: bigint = orderedEvent.sequence;
  const payload: Uint8Array = orderedEvent.data;
  const trackId: string = orderedEvent.trackId;
  void [sequence, payload, trackId];
} else {
  const from: bigint = orderedEvent.from;
  const to: bigint = orderedEvent.to;
  void [from, to];
}

declare const videoMediaTrack: MediaStreamTrack;
declare const audioMediaTrack: MediaStreamTrack;
const capturedVideo = createCaptureSource(videoMediaTrack, "video");
const capturedAudio = createCaptureSource(audioMediaTrack, "audio");
const localVideo = agent.track(track.video("camera"));
const localAudio = agent.track(track.audio("microphone"));
const videoOwner = attachLocalTrack(localVideo, {
  source: capturedVideo,
  enabled: false,
  config: { contentHint: "text" },
});
const audioOwner = attachLocalTrack(localAudio, {
  source: capturedAudio,
  config: { contentHint: "music" },
});
const videoReady: Promise<void> = videoOwner.ready;
videoOwner.setEnabled(true);
videoOwner.setSource(null);
const videoClosed: Promise<void> = videoOwner.close();
void [videoReady, videoClosed, audioOwner];
// @ts-expect-error Kind-incompatible capture cannot be published.
attachLocalTrack(localVideo, { source: capturedAudio });
// @ts-expect-error Video publication options reject audio content hints.
videoOwner.setConfig({ contentHint: "speech" });
const ownershipError: Error = new ResourceOwnershipError("already owned");
void ownershipError;

const remoteVideo: RemoteVideoTrack = participant.track(track.video("camera"));
const remoteAudio: RemoteAudioTrack = participant.track(
  track.audio("microphone"),
);
declare const videoElement: HTMLVideoElement;
const videoAttachment = attachRemoteVideo(
  remoteVideo,
  videoElement,
  undefined,
  {
    preferences: {
      minHeight: 360,
      minFps: 15,
      priority: 1,
      playoutDelay: { minMs: 0, maxMs: 100 },
    },
  },
);
const videoAttachmentReady: Promise<void> = videoAttachment.ready;
videoAttachment.setPreferences({ minHeight: 720 });
videoAttachment.close();
void videoAttachmentReady;
const audioAttachment = attachRemoteAudio(agent, {
  onPlaybackError(failure: PlaybackFailure) {
    void failure.retry();
  },
});
const audioReady: Promise<void> = audioAttachment.ready;
const audioClosed: Promise<void> = audioAttachment.close();
void [audioReady, audioClosed, remoteAudio];

const participants = agent.remote.participants;
const tracks = participant.tracks;
void [participants, tracks];
// @ts-expect-error Parallel kind-specific collection roots were removed.
agent.remote.videoTracks;
// @ts-expect-error Parallel kind-specific collection roots were removed.
participant.audioTracks;
// @ts-expect-error Imperative receive policy belongs to an attachment.
remoteVideo.setReceiveOptions({});
// @ts-expect-error Per-track audio analysis is not a normal ownership path.
remoteAudio.readWaveform(new Float32Array());
// @ts-expect-error Callable local roots were removed.
agent.local;
// @ts-expect-error Typed JSON topics were removed.
agent.topic("chat", { mode: "ordered" });
// @ts-expect-error Desired-state mutation was removed.
agent.setState({ connected: true });
// @ts-expect-error Retry uses connect(), not a second session operation.
agent.reconnect();
// @ts-expect-error Raw sender replacement is attachment-owned.
agent.replaceLocalTrack("v0", null, { contentHint: "motion" });
agent.close();
