import {
  AudioSource,
  VideoSource,
  Agent,
  attachRemoteVideo,
  attachRemoteAudio,
  ResourceOwnershipError,
  AgentStatus,
  ErrorCode,
  OperationError,
  type AgentState,
  type AgentUpdates,
  type VideoSubscriptionState,
  type AudioSubscriptionState,
  type VideoSubscriptionUpdates,
  type AudioSubscriptionUpdates,
  type ErrorInfo,
  type AgentError,
  type AgentSnapshot,
  type DataEvent,
  type DataUpdate,
  type DataPublication,
  type DataSubscription,
  type DataPublicationState,
  type DataSubscriptionState,
  type DataPublicationUpdates,
  type DataSubscriptionUpdates,
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
import type { DesiredState } from "../../web/index.js";
// @ts-expect-error JSON topic registration is not required for binary data.
import type { Topic } from "../../web/index.js";

const agent = new Agent({
  endpoint: "https://pulsebeam.example",
  token: "opaque",
});
agent.join("explicit-room");
const currentState: AgentState = agent.state();
const stateCursor: AgentUpdates = agent.updates();
const nextState: Promise<AgentState | undefined> = stateCursor.next();
const stateRevision: bigint = currentState.observationRevision;
const desiredJoinRevision: bigint = currentState.desiredJoinRevision;
const observedRoom: string | undefined = currentState.room;
const status: AgentStatus = currentState.status;
const operation = new OperationError({
  code: ErrorCode.Conflict,
  message: "Pending observation read",
});
const info: ErrorInfo = operation.info;
type NoAny<T> = 0 extends 1 & T ? false : true;
const stateTyped: NoAny<ReturnType<typeof agent.state>> = true;
const updateTyped: NoAny<Awaited<ReturnType<AgentUpdates["next"]>>> = true;
// @ts-expect-error State views are immutable generated values.
currentState.desiredRoom = "overwrite";
if (currentState.error) {
  // @ts-expect-error Nested error information is immutable too.
  currentState.error.message = "overwrite";
}
// @ts-expect-error Concrete cursors do not expose the foreign backend.
stateCursor.cursor;
stateCursor.close();
void [
  nextState,
  stateRevision,
  desiredJoinRevision,
  observedRoom,
  status,
  info,
  stateTyped,
  updateTyped,
];
const renewed: void = agent.renew("opaque-renewed");
void renewed;
const connected: Promise<void> = agent.connect();
await connected;
await agent.disconnect();
await agent.renewAuthorization("renewed");
agent.setLogging("info");
const snapshot: AgentSnapshot = agent.snapshot();
const joinRevision: bigint = snapshot.joinRevision;
const desiredRoom: string | null = snapshot.desiredRoom;
void joinRevision;
void desiredRoom;
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
// @ts-expect-error Agent-wide participant lookup is not room-pinned.
agent.participant("alice");

const binary = agent.data("chat", "ordered");
// @ts-expect-error Sending requires an owned publication.
binary.send(new Uint8Array([0, 255]));
// @ts-expect-error Reception requires a pinned remote subscription.
binary.receive();
const participant = agent.room("meeting").participant("alice");
const participantId: string = participant.id();
const remoteBinary = participant.data("chat", "ordered");
const dataPublication: DataPublication = binary.publish();
const dataSubscription: DataSubscription<"ordered"> = remoteBinary.subscribe();
const dataPublicationState: DataPublicationState = dataPublication.state();
const dataSubscriptionState: DataSubscriptionState = dataSubscription.state();
const dataPublicationUpdates: DataPublicationUpdates =
  dataPublication.updates();
const dataSubscriptionUpdates: DataSubscriptionUpdates =
  dataSubscription.updates();
const nextPublicationState: Promise<DataPublicationState | undefined> =
  dataPublicationUpdates.next();
const nextSubscriptionState: Promise<DataSubscriptionState | undefined> =
  dataSubscriptionUpdates.next();
const dataSend: Promise<void> = dataPublication.send(new Uint8Array([0, 255]));
const subscriptionNext: Promise<DataUpdate<"ordered">> =
  dataSubscription.next();
const publicationTyped: NoAny<ReturnType<typeof binary.publish>> = true;
const subscriptionTyped: NoAny<ReturnType<typeof remoteBinary.subscribe>> =
  true;
const dataNextTyped: NoAny<Awaited<ReturnType<typeof dataSubscription.next>>> =
  true;
const publicationStateTyped: NoAny<ReturnType<typeof dataPublication.state>> =
  true;
const subscriptionStateTyped: NoAny<ReturnType<typeof dataSubscription.state>> =
  true;
// @ts-expect-error Owned declarations are synchronous.
const deferredDataPublication: Promise<unknown> = dataPublication;
// @ts-expect-error Owned subscriptions are synchronous.
const deferredDataSubscription: Promise<unknown> = dataSubscription;
// @ts-expect-error Payloads remain binary.
dataPublication.send("text");
// @ts-expect-error Core selectors are readonly.
dataSubscriptionState.selector.label = "other";
dataPublicationUpdates.close();
dataSubscriptionUpdates.close();
await dataPublication.close();
await dataSubscription.close();
void [
  dataPublicationState,
  nextPublicationState,
  nextSubscriptionState,
  dataSend,
  subscriptionNext,
  publicationTyped,
  subscriptionTyped,
  dataNextTyped,
  publicationStateTyped,
  subscriptionStateTyped,
  deferredDataPublication,
  deferredDataSubscription,
];
// @ts-expect-error Reception is created synchronously with subscribe.
remoteBinary.receive();
const remoteReceiver = remoteBinary.subscribe();
const next: Promise<DataUpdate<"ordered">> = remoteReceiver.next();
await remoteReceiver.close();
void [next, participantId];
// @ts-expect-error Data delivery is explicit.
agent.data("chat");
// @ts-expect-error Binary payloads do not have an implicit string codec.
dataPublication.send("text");
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
const videoOwner = localVideo.publish({ enabled: false });
const audioOwner = localAudio.publish();
const videoPipe = videoSource.pipeTo(videoOwner);
const audioPipe = audioSource.pipeTo(audioOwner);
// @ts-expect-error Publication intent is not a Promise.
const deferredPublication: Promise<unknown> = videoOwner;
// @ts-expect-error Input attachment is not a Promise.
const deferredPipe: Promise<unknown> = videoPipe;
void [deferredPublication, deferredPipe];
videoOwner.configure({ enabled: true });
await videoOwner.configureSender({ contentHint: "text" });
await audioOwner.configureSender({ contentHint: "music" });
await videoPipe.close();
await audioPipe.close();
// @ts-expect-error Pipe requires the matching concrete publication kind.
audioSource.pipeTo(videoOwner);
// @ts-expect-error Publication does not accept an implicit source.
localAudio.publish(audioSource);
// @ts-expect-error Input lifetime belongs to a separate Pipe.
videoOwner.setSource(null);
// @ts-expect-error Video sender configuration rejects audio content hints.
videoOwner.configureSender({ contentHint: "speech" });
const ownershipError: Error = new ResourceOwnershipError("already owned");
void ownershipError;

const remoteVideo: RemoteVideoTrack = participant.video("camera");
const remoteAudio: RemoteAudioTrack = participant.audio("microphone");
const subscription = remoteVideo.subscribe({
  visible: true,
  height: 720,
  minHeight: 360,
  minFps: 15,
  priority: 1,
  playoutDelay: { mode: "fixed", minMs: 0, maxMs: 100 },
});
const videoState: VideoSubscriptionState = subscription.state();
const videoUpdates: VideoSubscriptionUpdates = subscription.updates();
const nextVideoState: Promise<VideoSubscriptionState | undefined> =
  videoUpdates.next();
const videoStateTyped: NoAny<ReturnType<typeof subscription.state>> = true;
const videoUpdateTyped: NoAny<
  Awaited<ReturnType<VideoSubscriptionUpdates["next"]>>
> = true;
// @ts-expect-error Core selector values are readonly.
videoState.selector.label = "other";
// @ts-expect-error The concrete cursor does not expose the foreign backend.
videoUpdates.cursor;
videoUpdates.close();
void [videoState, nextVideoState, videoStateTyped, videoUpdateTyped];
// @ts-expect-error Local subscription intent is not a Promise.
const deferredSubscription: Promise<unknown> = subscription;
void deferredSubscription;
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

const audioSubscription = remoteAudio.subscribe({
  playoutDelay: { mode: "adaptive" },
});
const audioState: AudioSubscriptionState = audioSubscription.state();
const audioUpdates: AudioSubscriptionUpdates = audioSubscription.updates();
const nextAudioState: Promise<AudioSubscriptionState | undefined> =
  audioUpdates.next();
const audioStateTyped: NoAny<ReturnType<typeof audioSubscription.state>> = true;
const audioUpdateTyped: NoAny<
  Awaited<ReturnType<AudioSubscriptionUpdates["next"]>>
> = true;
// @ts-expect-error Core selector values are readonly.
audioState.selector.label = "other";
// @ts-expect-error The concrete cursor does not expose the foreign backend.
audioUpdates.cursor;
audioUpdates.close();
void [audioState, nextAudioState, audioStateTyped, audioUpdateTyped];
// @ts-expect-error Local audio subscription intent is not a Promise.
const deferredAudioSubscription: Promise<unknown> = audioSubscription;
void deferredAudioSubscription;
await audioSubscription.close();
const selection = await agent.selectAudio({ automatic: true });
await selection.configure({
  automatic: false,
  selected: [{ room: "meeting", participantId: "alice", label: "microphone" }],
});
await selection.close();
void observedTrack;
// @ts-expect-error Receive ownership is not a reactive callback subscription.
remoteVideo.subscribe(() => {});
// @ts-expect-error Kind-agnostic descriptor lookup was removed.
participant.track({ kind: "video", label: "camera" });
// @ts-expect-error Local descriptor lookup was removed.
agent.track({ kind: "video", label: "camera" });
// @ts-expect-error The removed Agent.remote alias cannot bypass room pinning.
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
