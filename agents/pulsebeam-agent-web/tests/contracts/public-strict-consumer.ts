import {
  AudioSource,
  VideoSource,
  createAgent,
  attachRemoteVideo,
  attachRemoteAudio,
  ResourceOwnershipError,
  type AgentError,
  type AgentSnapshot,
  type DataEvent,
  type DataUpdate,
  type SnapshotUpdate,
  type EventUpdate,
  type RemoteAudioTrack,
  type RemoteVideoTrack,
} from "../../web/index.js";

// @ts-expect-error Managed activation is an internal React lifecycle.
import { createManagedAgent } from "../../web/index.js";
// @ts-expect-error There is one public constructor spelling.
import { createWebAgent } from "../../web/index.js";
// @ts-expect-error Descriptors are not a parallel public lookup path.
import { track } from "../../web/index.js";
// @ts-expect-error Desired-state mutation is not a normal public API.
import type { AgentState } from "../../web/index.js";
// @ts-expect-error JSON topic registration is not required for binary data.
import type { Topic } from "../../web/index.js";

const agent = await createAgent({
  endpoint: "https://pulsebeam.example",
  token: "opaque",
});
const connected: Promise<void> = agent.connect();
await connected;
await agent.disconnect();
await agent.renewAuthorization("renewed");
agent.setLogging("info");
const snapshot: AgentSnapshot = agent.snapshot();
const error: AgentError | null = agent.error;
const snapshots = agent.snapshots();
const events = agent.events();
const observedSnapshot: SnapshotUpdate = await snapshots.next();
const observedEvent: Promise<EventUpdate> = events.next();
await snapshots.close();
await events.close();
const observationsClosed: boolean = snapshots.isClosed() && events.isClosed();
void [snapshot, error, observedSnapshot, observedEvent, observationsClosed];
// @ts-expect-error Snapshot cursors do not expose their writable channel.
snapshots.observation;
// @ts-expect-error Observation is an owned cursor, not a callback alias.
agent.subscribe(() => {});
// @ts-expect-error Current snapshot has one common spelling.
agent.getSnapshot();

const binary = agent.data("chat", "ordered");
const binarySend: Promise<void> = binary.send(new Uint8Array([0, 255]));
const receiver = await binary.receive();
const participant = agent.participant("alice");
const participantId: string = participant.id();
const remoteBinary = participant.data("chat", "ordered");
const remoteReceiver = await remoteBinary.receive();
const next: Promise<DataUpdate<"ordered">> = remoteReceiver.next();
await receiver.close();
await remoteReceiver.close();
void [binarySend, next, participantId];
// @ts-expect-error Data delivery is explicit.
agent.data("chat");
// @ts-expect-error Binary payloads do not have an implicit string codec.
binary.send("text");
// @ts-expect-error Remote data handles are receive-only.
remoteBinary.send(new Uint8Array());
// @ts-expect-error Explicit next/close cursors replace automatic iterators.
binary.events();
declare const orderedEvent: DataEvent<"ordered">;
if (orderedEvent.type === "message") {
  const sequence: bigint = orderedEvent.sequence;
  const payload: Uint8Array = orderedEvent.data;
  void [sequence, payload];
} else {
  const from: bigint = orderedEvent.from;
  const to: bigint = orderedEvent.to;
  void [from, to];
}

declare const videoMediaTrack: MediaStreamTrack;
declare const audioMediaTrack: MediaStreamTrack;
const videoSource = VideoSource.fromTrack(videoMediaTrack);
const audioSource = AudioSource.fromTrack(audioMediaTrack);
const localVideo = agent.video("camera");
const localAudio = agent.audio("microphone");
const videoOwner = await localVideo.publish(videoSource, { enabled: false });
const audioOwner = await localAudio.publish(audioSource, { enabled: true });
await videoOwner.configure({ enabled: true });
await videoOwner.configureSender({ contentHint: "text" });
await audioOwner.configureSender({ contentHint: "music" });
await videoOwner.setSource(null);
await videoOwner.setSource(videoSource);
// @ts-expect-error Publication requires the matching concrete source kind.
localVideo.publish(audioSource, { enabled: true });
// @ts-expect-error Publication requires an explicit ordinary configuration.
localAudio.publish(audioSource);
// @ts-expect-error Video sender configuration rejects audio content hints.
videoOwner.configureSender({ contentHint: "speech" });
const ownershipError: Error = new ResourceOwnershipError("already owned");
void ownershipError;

const remoteVideo: RemoteVideoTrack = participant.video("camera");
const remoteAudio: RemoteAudioTrack = participant.audio("microphone");
const subscription = await remoteVideo.subscribe({
  visible: true,
  height: 720,
  minHeight: 360,
  minFps: 15,
  priority: 1,
  playoutDelay: { mode: "fixed", minMs: 0, maxMs: 100 },
});
const observation = await subscription.observe();
const observedTrack: MediaStreamTrack | null = observation.track();
await observation.close();
await subscription.close();
const presentation = attachRemoteVideo(
  remoteVideo,
  document.createElement("video"),
  undefined,
  { preferences: { minHeight: 360 } },
);
const playback = attachRemoteAudio(agent);
await presentation.ready;
await playback.ready;
const retiredPresentation: Promise<void> = presentation.close();
await playback.close();
void retiredPresentation;
// @ts-expect-error Presentation does not require application receiver bookkeeping.
presentation.setPublicationIds([]);

const audioSubscription = await remoteAudio.subscribe({
  playoutDelay: { mode: "adaptive" },
});
await audioSubscription.close();
const selection = await agent.selectAudio({ automatic: true });
await selection.configure({
  automatic: false,
  selected: [{ participantId: "alice", label: "microphone" }],
});
await selection.close();
void observedTrack;
// @ts-expect-error Receive ownership is not a reactive callback subscription.
remoteVideo.subscribe(() => {});
// @ts-expect-error Kind-agnostic descriptor lookup was removed.
participant.track({ kind: "video", label: "camera" });
// @ts-expect-error Local descriptor lookup was removed.
agent.track({ kind: "video", label: "camera" });
// @ts-expect-error Remote lookup has one direct typed entry point.
agent.remote.participant("alice");
// @ts-expect-error Raw desired state cannot override owned resources.
agent.setState({ connected: true });
// @ts-expect-error Public callers do not control physical receiver slots.
remoteVideo.setReceiveOptions({});
// @ts-expect-error JSON topic registration is not a normal data path.
agent.topic("chat", { mode: "ordered" });
await videoOwner.close();
await audioOwner.close();
const producerClosed: boolean = videoSource.isClosed();
await videoSource.close();
await audioSource.close();
const closed: Promise<void> = agent.close();
await closed;
const terminal: boolean = agent.isClosed();
void [producerClosed, terminal];
