import {
  AgentConfig,
  AgentStatus,
  AgentUpdates,
  ErrorCode,
  AudioDemand,
  AudioSubscriptionConfig,
  DataUpdate,
  PlayoutDelay,
  PublicationConfig,
  PublicationAdmissionStatus,
  PublicationDemand,
  PublicationUpdates,
  LogLevel,
  ResourceFailure,
  RetryPolicy,
  TopicMode,
  VideoObservation,
  Agent,
  RoomRef,
  Watch,
  WatchConfig,
  WatchStatus,
  WatchUpdates,
  AudioSource,
  AudioFrames,
  VideoFrames,
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
for (const config of [
  AgentConfig.create({ endpoint: "relative", token: "secret-token", retry: RetryPolicy.create({ initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 2 }), logLevel: LogLevel.Warn }),
  AgentConfig.create({ endpoint: "http://pulsebeam.test", token: "", retry: RetryPolicy.create({ initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 2 }), logLevel: LogLevel.Warn }),
  AgentConfig.create({ endpoint: "http://pulsebeam.test", token: "secret-token", retry: RetryPolicy.create({ initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 0 }), logLevel: LogLevel.Warn }),
]) {
  try {
    new Agent(config);
    throw new Error("invalid Agent config was accepted");
  } catch (error) {
    if (!NativeError.Operation.instanceOf(error) ||
        error.inner.info.code !== ErrorCode.InvalidArgument ||
        error.inner.info.message !== "Invalid Agent configuration" ||
        error.inner.info.message.includes("secret-token")) throw error;
  }
}

const agent = new Agent(
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
if (agent instanceof Promise) throw new Error("Agent construction was asynchronous");
const initialState = agent.state();
const stateUpdates = agent.updates();
if (!AgentUpdates.instanceOf(stateUpdates) ||
    initialState.status !== AgentStatus.Idle ||
    initialState.room !== undefined || initialState.desiredRoom !== undefined ||
    typeof initialState.observationRevision !== "bigint" ||
    (await stateUpdates.next())?.observationRevision !== initialState.observationRevision) {
  throw new Error("missing initial concrete Agent state");
}
if ("catalog" in agent.snapshot() || "mapping" in agent.snapshot()) {
  throw new Error("snapshot retained joined-room discovery metadata");
}
if (false) {
  // @ts-expect-error Discovery is available only through an owned room watch.
  agent.snapshot().catalog;
  // @ts-expect-error Receiver mappings are private runtime reconciliation state.
  agent.snapshot().mapping;
}
const revision = agent.snapshot().desiredRevision;
const joinRevision: bigint = agent.snapshot().joinRevision;
const credentialRevision: bigint = agent.snapshot().credentialRevision;
const acceptedCredentialRevision: bigint | undefined = agent.snapshot().acceptedCredentialRevision;
if (agent.snapshot().credentialOutcome !== undefined) {
  throw new Error("inert agent evaluated credentials");
}
if (credentialRevision !== 0n || acceptedCredentialRevision !== undefined) {
  throw new Error("inert agent acquired accepted credentials");
}
const desiredRoom: string | undefined = agent.snapshot().desiredRoom;
if (joinRevision !== 0n || desiredRoom !== undefined) {
  throw new Error("inert agent acquired room intent");
}
const renewed: void = agent.renew("renewed-token");
if (renewed !== undefined || agent.snapshot().credentialRevision !== 1n || agent.snapshot().desiredRoom !== undefined) {
  throw new Error("renew did not synchronously commit inert credential intent");
}
const localData = agent.data("control", TopicMode.Ordered);
const room = agent.room("room");
if (!RoomRef.instanceOf(room) || room.name() !== "room" || "participant" in agent || "receive" in localData) {
  throw new Error("reference factory retained a room-wide receive surface");
}
const watch = room.watch();
const watchState = watch.state();
const watchUpdates = watch.updates();
if (!Watch.instanceOf(watch) || !WatchUpdates.instanceOf(watchUpdates) ||
    watchState.room !== "room" || watchState.limit !== 100 ||
    watchState.status !== WatchStatus.Loading || watchState.page !== undefined ||
    (await watchUpdates.next())?.observationRevision !== watchState.observationRevision) {
  throw new Error("missing concrete default catalog watch state");
}
watch.close();
watch.close();
if ((await watchUpdates.next())?.status !== WatchStatus.Closed ||
    await watchUpdates.next() !== undefined) {
  throw new Error("watch close did not expose one final state");
}
watchUpdates.close();
const scopedWatch = room.watch(WatchConfig.create({ limit: 3, cursor: new Uint8Array([7, 8]) }));
if (scopedWatch.state().limit !== 3 || scopedWatch.state().cursor[0] !== 7 ||
    scopedWatch.state().cursor[1] !== 8) throw new Error("watch scope was not retained");
scopedWatch.close();
const participant = room.participant("remote");
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
const videoPublication = localVideo.publish();
if (videoPublication instanceof Promise || !videoPublication.state().configuration.enabled) {
  throw new Error("zero-argument publish was not synchronous with RFC enabled default");
}
if (false) {
  // @ts-expect-error An audio source is not a video publication source.
  await videoPublication.setSource(audioSource);
}
await videoPublication.setSource(videoSource);
const publicationUpdates = videoPublication.updates();
const initialPublication = videoPublication.state();
if (!PublicationUpdates.instanceOf(publicationUpdates) ||
    initialPublication.admission.status !== PublicationAdmissionStatus.Pending ||
    initialPublication.admission.demand !== PublicationDemand.Unknown ||
    initialPublication.configuration.enabled !== true ||
    typeof initialPublication.configuration.desiredRevision !== "bigint" ||
    (await publicationUpdates.next())?.observationRevision !== initialPublication.observationRevision) {
  throw new Error("publication did not expose pending Core state and local configuration");
}
try {
  sameVideo.publish();
  throw new Error("equivalent lookup bypassed publication exclusivity");
} catch (error) {
  if (!NativeError.Resource.instanceOf(error) ||
      !ResourceFailure.AlreadyOwned.instanceOf(error.inner.reason)) throw error;
}
await videoPublication.configure(PublicationConfig.create({ enabled: false }));
const configuredPublication = videoPublication.state();
if (configuredPublication.admission.status !== PublicationAdmissionStatus.Pending ||
    configuredPublication.admission.demand !== PublicationDemand.Unknown ||
    configuredPublication.configuration.enabled !== false ||
    configuredPublication.configuration.desiredRevision <= initialPublication.configuration.desiredRevision ||
    (await publicationUpdates.next())?.configuration.enabled !== false) {
  throw new Error("publication configuration was confused with admission or demand");
}
const noOpRevision = configuredPublication.configuration.desiredRevision;
await videoPublication.configure(PublicationConfig.create({ enabled: false }));
if (videoPublication.state().configuration.desiredRevision !== noOpRevision) {
  throw new Error("no-op configuration advanced its durable revision");
}
await videoPublication.configure(PublicationConfig.create({ enabled: true }));
if (!videoPublication.state().configuration.enabled) throw new Error("latest configuration setter was lost");
await videoPublication.configure(PublicationConfig.create({ enabled: false }));
if (videoPublication.state().configuration.enabled) throw new Error("latest configuration setter was lost");
await videoPublication.setSource(undefined);
await videoPublication.setSource(videoSource);
const pendingBound = videoPublication.ready().then(
  () => { throw new Error("publication without a native binding became ready"); },
  (error: unknown) => error,
);
await videoPublication.close();
if (!NativeError.Closed.instanceOf(await pendingBound) || videoSource.isClosed() ||
    !videoPublication.state().closed || (await publicationUpdates.next())?.closed !== true ||
    (await publicationUpdates.next()) !== undefined) {
  throw new Error("publication close transferred producer ownership");
}
publicationUpdates.close();
const videoReplacement = sameVideo.publish(publicationConfig);
await videoReplacement.setSource(videoSource);
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
const audioPublication = localAudio.publish();
if (audioPublication instanceof Promise || !audioPublication.state().configuration.enabled) {
  throw new Error("zero-argument audio publish was not synchronous with RFC enabled default");
}
await audioPublication.setSource(audioSource);
const audioPublicationUpdates = audioPublication.updates();
const initialAudioPublication = await audioPublicationUpdates.next();
if (!initialAudioPublication || initialAudioPublication.admission.status !== PublicationAdmissionStatus.Pending) {
  throw new Error("audio publication did not start pending");
}
const pendingAudioPublication = audioPublicationUpdates.next();
try {
  await audioSource.submit(InputFrame.Pcm.new({ frame: PcmFrame.create({
    sampleRateHz: 48000, channels: 1, samples: [-32768, 0, 32767], timestampUs: 0n,
  }) }));
  throw new Error("unallocated input accepted a frame");
} catch (error) {
  if (!NativeError.NotReady.instanceOf(error)) throw error;
}
const explicitVideo = remoteVideo.subscribe(VideoObservation.create({
  visible: false, height: 0, minHeight: 0, minFps: 0, priority: 1,
  playoutDelay: PlayoutDelay.Adaptive.new(),
}));
await explicitVideo.close();
explicitVideo.uniffiDestroy();
const explicitAudio = remoteAudio.subscribe(AudioSubscriptionConfig.create({
  playoutDelay: PlayoutDelay.Adaptive.new(),
}));
await explicitAudio.close();
explicitAudio.uniffiDestroy();
const videoSubscription = remoteVideo.subscribe();
if (videoSubscription instanceof Promise) throw new Error("video subscribe must be synchronous");
const audioSubscription = remoteAudio.subscribe();
if (audioSubscription instanceof Promise) throw new Error("audio subscribe must be synchronous");
const videoState = videoSubscription.state();
const audioState = audioSubscription.state();
const videoUpdates = videoSubscription.updates();
const audioUpdates = audioSubscription.updates();
if (!videoState.configuration.visible || videoState.configuration.height !== 0 ||
    videoState.configuration.minHeight !== 0 || videoState.configuration.minFps !== 0 ||
    videoState.configuration.priority !== 0 ||
    !PlayoutDelay.Adaptive.instanceOf(videoState.configuration.playoutDelay) ||
    !PlayoutDelay.Adaptive.instanceOf(audioState.playoutDelay) ||
    (await videoUpdates.next())?.observationRevision !== videoState.observationRevision ||
    (await audioUpdates.next())?.observationRevision !== audioState.observationRevision) {
  throw new Error("subscription defaults or current-first state were lost");
}
const audioSelection = await agent.selectAudio(AudioDemand.create({
  pinned: [], selected: [], automatic: false, playoutDelays: [], selectorDelays: [],
}));
await audioSelection.close();
if (audioSubscription.isClosed()) throw new Error("selection close retired named audio demand");
const frameReaders: (VideoFrames | AudioFrames)[] = [];
const pendingFrames: Promise<unknown>[] = [];
for (const [subscription, Reader] of [
  [videoSubscription, VideoFrames], [audioSubscription, AudioFrames],
] as const) {
  const reader = subscription.frames();
  if (reader instanceof Promise || !Reader.instanceOf(reader)) throw new Error("frames must return a concrete reader synchronously");
  try {
    subscription.frames();
    throw new Error("second active frame reader was admitted");
  } catch (error) {
    if (!NativeError.Operation.instanceOf(error) || error.inner.info.code !== ErrorCode.Conflict) throw error;
  }
  const reads = [reader.next(), reader.next()];
  const first = await Promise.race(reads.map((read, index) => read.then(
    () => ({ index, error: undefined }),
    (error: unknown) => ({ index, error }),
  )));
  if (!NativeError.Operation.instanceOf(first.error) || first.error.inner.info.code !== ErrorCode.Conflict) {
    throw new Error("concurrent frame read did not fail with Conflict", { cause: first.error });
  }
  if (reader.close() !== undefined || await reads[1 - first.index] !== undefined) {
    throw new Error("reader close did not end pending read");
  }
  if (subscription.isClosed()) throw new Error("reader close retired subscription");
  reader.uniffiDestroy();
  const replacement = subscription.frames();
  if (replacement instanceof Promise || !Reader.instanceOf(replacement)) throw new Error("reader was not replaced synchronously");
  frameReaders.push(replacement);
  pendingFrames.push(replacement.next());
}
const pendingMedia = videoSubscription.observe(ReceiveMode.Decoded).then(
  () => { throw new Error("absent source produced a media observation"); },
  (error: unknown) => error,
);
if ("send" in localData || "receive" in remoteData) {
  throw new Error("inert data addresses exposed payload operations");
}
if (false) {
  // @ts-expect-error Sending requires an owned publication.
  localData.send(new Uint8Array());
  // @ts-expect-error Reception requires an owned subscription.
  remoteData.receive();
}
const dataPublication = localData.publish();
const dataState = dataPublication.state();
const dataUpdates = dataPublication.updates();
const siblingDataUpdates = dataPublication.updates();
if (dataPublication instanceof Promise || dataState.label !== "control" ||
    dataState.delivery !== TopicMode.Ordered ||
    (await dataUpdates.next())?.observationRevision !== dataState.observationRevision ||
    (await siblingDataUpdates.next())?.observationRevision !== dataState.observationRevision) {
  throw new Error("data publication did not expose independent current-first cursors");
}
dataUpdates.close();
try {
  agent.data("control", TopicMode.Ordered).publish();
  throw new Error("duplicate data publication was admitted");
} catch (error) {
  if (!NativeError.Resource.instanceOf(error) ||
      !ResourceFailure.AlreadyOwned.instanceOf(error.inner.reason)) throw error;
}
try {
  await dataPublication.send(new Uint8Array([0, 255]));
  throw new Error("disconnected data send was admitted");
} catch (error) {
  if (
    !NativeError.Operation.instanceOf(error) ||
    error.inner.info.code !== ErrorCode.NotReady
  ) {
    throw error;
  }
}
await dataPublication.close();
if (!dataPublication.state().closed || !(await siblingDataUpdates.next())?.closed ||
    await siblingDataUpdates.next() !== undefined) {
  throw new Error("data publication did not retain its final state");
}
siblingDataUpdates.close();
try {
  await dataPublication.send(new Uint8Array([0, 255]));
  throw new Error("closed data publication accepted a send");
} catch (error) {
  if (!NativeError.Operation.instanceOf(error) || error.inner.info.code !== ErrorCode.Closed) throw error;
}
const replacementData = localData.publish();
await replacementData.close();
const closedCursor = remoteData.subscribe();
const independentCursor = remoteData.subscribe();
const participantCursor = remoteData.subscribe();
if (closedCursor instanceof Promise || independentCursor instanceof Promise || participantCursor instanceof Promise) {
  throw new Error("data subscribe was asynchronous");
}
const receiverState = closedCursor.state();
const receiverUpdates = closedCursor.updates();
if ((await receiverUpdates.next())?.observationRevision !== receiverState.observationRevision) {
  throw new Error("data receiver did not expose current-first state");
}
const pendingClose = closedCursor.next();
await closedCursor.close();
await closedCursor.close();
if (
  !DataUpdate.Closed.instanceOf(await pendingClose) ||
  !closedCursor.isClosed()
) {
  throw new Error("cursor close did not complete its read");
}
if (!closedCursor.state().closed || !(await receiverUpdates.next())?.closed ||
    await receiverUpdates.next() !== undefined) {
  throw new Error("data receiver did not expose its final state");
}
receiverUpdates.close();
if (independentCursor.isClosed() || participantCursor.isClosed()) {
  throw new Error("cursor close retired an independent receiver");
}
const pendingIndependent = independentCursor.next();
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
if (agent.isClosed() || independentCursor.isClosed() || participantCursor.isClosed()) {
  throw new Error("disconnect was terminal");
}
await agent.close();
await agent.close();
if (!agent.isClosed()) throw new Error("close did not retire the Agent");
for (const [index, reader] of frameReaders.entries()) {
  if (await pendingFrames[index] !== undefined || await reader.next() !== undefined || reader.close() !== undefined) {
    throw new Error("subscription close did not finish frame reader");
  }
  reader.uniffiDestroy();
}
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
  !DataUpdate.Closed.instanceOf(await pendingIndependent) ||
  !DataUpdate.Closed.instanceOf(await pendingParticipant)
) {
  throw new Error("Agent close did not complete owned data reads");
}
try {
  const joined: void = agent.join("room");
  void joined;
  throw new Error("closed Agent revived");
} catch (error) {
  if (!NativeError.Operation.instanceOf(error) || error.inner.info.code !== ErrorCode.Closed) throw error;
}
if (!videoSubscription.state().closed || !(await videoUpdates.next())?.closed ||
    await videoUpdates.next() !== undefined || !audioSubscription.state().closed ||
    !(await audioUpdates.next())?.closed || await audioUpdates.next() !== undefined) {
  throw new Error("terminal Agent close did not finish subscription state cursors");
}
videoUpdates.close();
audioUpdates.close();
videoUpdates.uniffiDestroy();
audioUpdates.uniffiDestroy();
receiverUpdates.uniffiDestroy();
dataUpdates.uniffiDestroy();
siblingDataUpdates.uniffiDestroy();
dataPublication.uniffiDestroy();
replacementData.uniffiDestroy();
const finalAudioPublication = await pendingAudioPublication;
if (!finalAudioPublication?.closed || await audioPublicationUpdates.next() !== undefined ||
    !audioPublication.state().closed) {
  throw new Error("terminal Agent close did not finish a retained publication cursor");
}
audioPublicationUpdates.close();
const finalState = await stateUpdates.next();
if (finalState?.status !== AgentStatus.Closed ||
    finalState.desiredCredentialRevision !== agent.snapshot().credentialRevision ||
    initialState.status !== AgentStatus.Idle ||
    (await stateUpdates.next()) !== undefined ||
    agent.state().observationRevision !== finalState.observationRevision) {
  throw new Error("Agent state did not retain one immutable final close");
}
stateUpdates.close();
stateUpdates.close();
stateUpdates.uniffiDestroy();
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
independentCursor.uniffiDestroy();
participantCursor.uniffiDestroy();
remoteData.uniffiDestroy();
participant.uniffiDestroy();
localData.uniffiDestroy();
events.uniffiDestroy();
snapshots.uniffiDestroy();
agent.uniffiDestroy();
console.log("generated Node lifecycle contract PASS");
