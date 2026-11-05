import {
  AgentConfig,
  AudioDemand,
  AudioSubscriptionConfig,
  DataUpdate,
  PlayoutDelay,
  PublicationConfig,
  LogLevel,
  ResourceFailure,
  RetryPolicy,
  TopicMode,
  VideoObservation,
  Agent,
  AudioSource,
  Encoding,
  EventStream,
  FeedbackStream,
  FeedbackUpdate,
  InputFrame,
  PcmFrame,
  ReceiveMode,
  EventUpdate,
  NativeError,
  SnapshotStream,
  SnapshotUpdate,
  VideoSource,
  VideoSourceConfig,
} from "../node_assets/index.js";

// No connection intent: this witnesses generated objects and async disposal,
// not network admission or media delivery. Production-native covers that boundary.
const agent = await Agent.create(
  AgentConfig.create({
    endpoint: "http://pulsebeam.test",
    token: "token",
    retry: RetryPolicy.create({
      initialDelayMs: 100n,
      maximumDelayMs: 1000n,
      maximumAttempts: 2,
    }),
    logLevel: LogLevel.Warn,
  }),
);
const revision = agent.snapshot().desiredRevision;
const localData = agent.data("control", TopicMode.Ordered);
const participant = agent.participant("remote");
const remoteData = participant.data("control", TopicMode.Ordered);
const localVideo = agent.video("camera");
const sameVideo = agent.video("camera");
const localAudio = agent.audio("microphone");
const remoteVideo = participant.video("camera");
const remoteAudio = participant.audio("microphone");
if (
  participant.id() !== "remote" ||
  agent.snapshot().desiredRevision !== revision
) {
  throw new Error("logical lookup acquired transport demand");
}
const videoSource = await agent.createVideoInput(VideoSourceConfig.create({
  encodings: [Encoding.create({
    rid: undefined, scaleDownBy: 1, maxBitrateBps: undefined,
    maxFramerate: undefined, temporalLayers: 1,
  })],
  dependencyDescriptor: true,
}));
const audioSource = await agent.createAudioInput();
if (!VideoSource.instanceOf(videoSource) || !AudioSource.instanceOf(audioSource) ||
    agent.snapshot().desiredRevision !== revision) {
  throw new Error("source construction acquired publication intent");
}
const publicationConfig = PublicationConfig.create({ enabled: true });
if (false) {
  // @ts-expect-error An audio source is not a video publication source.
  await localVideo.publish(audioSource, publicationConfig);
}
const videoPublication = await localVideo.publish(videoSource, publicationConfig);
try {
  await sameVideo.publish(videoSource, publicationConfig);
  throw new Error("equivalent lookup bypassed publication exclusivity");
} catch (error) {
  if (!NativeError.Resource.instanceOf(error) ||
      !ResourceFailure.AlreadyOwned.instanceOf(error.inner.reason)) throw error;
}
await videoPublication.configure(PublicationConfig.create({ enabled: false }));
await videoPublication.setSource(undefined);
await videoPublication.setSource(videoSource);
const pendingBound = videoPublication.ready().then(
  () => { throw new Error("publication without a native binding became ready"); },
  (error: unknown) => error,
);
await videoPublication.close();
if (!NativeError.Closed.instanceOf(await pendingBound) || videoSource.isClosed()) {
  throw new Error("publication close transferred producer ownership");
}
const videoReplacement = await sameVideo.publish(videoSource, publicationConfig);
const feedback = videoSource.feedback();
const siblingFeedback = videoSource.feedback();
if (!FeedbackStream.instanceOf(feedback)) throw new Error("feedback is not a generated object");
const pendingFeedback = feedback.next();
await feedback.close();
await feedback.close();
if (!feedback.isClosed() || !FeedbackUpdate.Closed.instanceOf(await pendingFeedback) ||
    siblingFeedback.isClosed() || videoSource.isClosed()) {
  throw new Error("feedback cursor close transferred producer or sibling ownership");
}
await siblingFeedback.close();
feedback.uniffiDestroy();
siblingFeedback.uniffiDestroy();
await videoSource.close();
if (videoReplacement.isClosed()) throw new Error("source stop closed publication ownership");
await videoReplacement.close();
const audioPublication = await localAudio.publish(audioSource, publicationConfig);
try {
  await audioSource.submit(InputFrame.Pcm.new({ frame: PcmFrame.create({
    sampleRateHz: 48000, channels: 1, samples: [-32768, 0, 32767], timestampUs: 0n,
  }) }));
  throw new Error("unallocated input accepted a frame");
} catch (error) {
  if (!NativeError.NotReady.instanceOf(error)) throw error;
}
const videoSubscription = await remoteVideo.subscribe(VideoObservation.create({
  visible: false, height: 0, minHeight: 0, minFps: 0, priority: 1,
  playoutDelay: PlayoutDelay.Adaptive.new(),
}));
const audioSubscription = await remoteAudio.subscribe(AudioSubscriptionConfig.create({
  playoutDelay: PlayoutDelay.Adaptive.new(),
}));
const audioSelection = await agent.selectAudio(AudioDemand.create({
  pinned: [], selected: [], automatic: false, playoutDelays: [], selectorDelays: [],
}));
await audioSelection.close();
if (audioSubscription.isClosed()) throw new Error("selection close retired named audio demand");
const pendingMedia = videoSubscription.observe(ReceiveMode.Decoded).then(
  () => { throw new Error("absent source produced a media observation"); },
  (error: unknown) => error,
);
try {
  await localData.send(new Uint8Array([0, 255]));
  throw new Error("disconnected data send was admitted");
} catch (error) {
  if (
    !NativeError.Resource.instanceOf(error) ||
    !ResourceFailure.NotConnected.instanceOf(error.inner.reason)
  ) {
    throw error;
  }
}
const closedCursor = await localData.receive();
const roomCursor = await localData.receive();
const participantCursor = await remoteData.receive();
const pendingClose = closedCursor.next();
await closedCursor.close();
await closedCursor.close();
if (
  !DataUpdate.Closed.instanceOf(await pendingClose) ||
  !closedCursor.isClosed()
) {
  throw new Error("cursor close did not complete its read");
}
if (roomCursor.isClosed() || participantCursor.isClosed()) {
  throw new Error("cursor close retired an independent receiver");
}
const pendingRoom = roomCursor.next();
const pendingParticipant = participantCursor.next();
const snapshots = agent.snapshots();
const events = agent.events();
if (
  !Agent.instanceOf(agent) ||
  !SnapshotStream.instanceOf(snapshots) ||
  !EventStream.instanceOf(events)
) {
  throw new Error("generated factory did not return concrete objects");
}
if (!SnapshotUpdate.Snapshot.instanceOf(await snapshots.next())) {
  throw new Error("missing initial generated snapshot");
}
const retiredSnapshots = agent.snapshots();
const retiredEvents = agent.events();
await retiredSnapshots.next();
const pendingRetiredSnapshot = retiredSnapshots.next();
await retiredSnapshots.close();
await retiredSnapshots.close();
await retiredEvents.close();
await retiredEvents.close();
if (!retiredSnapshots.isClosed() || !retiredEvents.isClosed() ||
    !SnapshotUpdate.Closed.instanceOf(await pendingRetiredSnapshot) ||
    !EventUpdate.Closed.instanceOf(await retiredEvents.next()) ||
    snapshots.isClosed() || events.isClosed() || agent.isClosed()) {
  throw new Error("observation cursor retirement affected an independent owner");
}
retiredSnapshots.uniffiDestroy();
retiredEvents.uniffiDestroy();
const pendingSnapshot = snapshots.next();
const pendingEvent = events.next();
await agent.disconnect();
if (agent.isClosed() || roomCursor.isClosed() || participantCursor.isClosed()) {
  throw new Error("disconnect was terminal");
}
await agent.close();
await agent.close();
if (!agent.isClosed()) throw new Error("close did not retire the Agent");
if (!audioSource.isClosed() || !audioPublication.isClosed() ||
    !audioSubscription.isClosed() || !videoSubscription.isClosed() ||
    !NativeError.Closed.instanceOf(await pendingMedia)) {
  throw new Error("terminal close did not retire media owners and pending acquisition");
}
if (!SnapshotUpdate.Closed.instanceOf(await pendingSnapshot)) {
  throw new Error("terminal close did not finish a pending snapshot read");
}
if (!EventUpdate.Closed.instanceOf(await pendingEvent)) {
  throw new Error("terminal close did not finish a pending event read");
}
if (
  !DataUpdate.Closed.instanceOf(await pendingRoom) ||
  !DataUpdate.Closed.instanceOf(await pendingParticipant)
) {
  throw new Error("Agent close did not complete owned data reads");
}
try {
  await agent.connect();
  throw new Error("closed Agent revived");
} catch (error) {
  if (!NativeError.Closed.instanceOf(error)) throw error;
}
audioSelection.uniffiDestroy();
audioSubscription.uniffiDestroy();
videoSubscription.uniffiDestroy();
audioPublication.uniffiDestroy();
videoReplacement.uniffiDestroy();
videoPublication.uniffiDestroy();
audioSource.uniffiDestroy();
videoSource.uniffiDestroy();
remoteAudio.uniffiDestroy();
remoteVideo.uniffiDestroy();
localAudio.uniffiDestroy();
sameVideo.uniffiDestroy();
localVideo.uniffiDestroy();
closedCursor.uniffiDestroy();
roomCursor.uniffiDestroy();
participantCursor.uniffiDestroy();
remoteData.uniffiDestroy();
participant.uniffiDestroy();
localData.uniffiDestroy();
events.uniffiDestroy();
snapshots.uniffiDestroy();
agent.uniffiDestroy();
console.log("generated Node lifecycle contract PASS");
