globalThis.__pulsebeamPublic = (async () => {
  const exports = Object.keys(window.pulsebeam).sort();
  const warnings = [];
  const consoleWarn = console.warn;
  console.warn = (...values) => warnings.push(values.map(String).join(" "));
  let endpointReads = 0;
  let tokenReads = 0;
  const config = {
    get endpoint() {
      endpointReads += 1;
      return location.origin;
    },
    get token() {
      tokenReads += 1;
      return "contract-token";
    },
    topology: {
      localVideos: 1,
      localAudios: 1,
      remoteVideos: 1,
      remoteAudios: 1,
    },
    logging: { level: "debug" },
  };
  const first = window.pulsebeam.createAgent(config);
  const second = window.pulsebeam.createAgent({
    endpoint: location.origin,
    token: "closed-token",
    topology: {},
  });
  const silent = window.pulsebeam.createAgent({
    endpoint: location.origin,
    token: "silent-token",
    topology: {},
    logging: { level: "off" },
  });
  const initial = first.getSnapshot();
  const initialStable = initial === first.getSnapshot();
  const initialFrozen =
    Object.isFrozen(initial) &&
    Object.isFrozen(initial.participants) &&
    Object.isFrozen(initial.publications) &&
    Object.isFrozen(initial.tracks) &&
    Object.isFrozen(initial.topics);

  second.close();
  const publications = [{ slot: "v0", label: "camera", active: true }];
  first.setState({ connected: true, publications });
  publications.push({ slot: "mutated", label: "mutated", active: true });
  first.setState({
    connected: false,
    topics: [{ name: "presence", mode: "latest", publish: true }],
  });

  const canvas = document.createElement("canvas");
  const track = canvas.captureStream(1).getVideoTracks()[0];
  const handleAgent = window.pulsebeam.createAgent({
    endpoint: location.origin,
    token: "handle-token",
    topology: { localVideos: 1, localAudios: 1 },
  });
  const videoHandle = handleAgent.local.video("camera");
  const sameVideoHandle = handleAgent.local.video("camera");
  const audioHandle = handleAgent.local.audio("camera");
  const capture = window.pulsebeam.createCaptureSource(track, "video");
  let capacity;
  try {
    handleAgent.local.video("screen");
  } catch (error) {
    capacity = error;
  }
  let handleChanges = 0;
  const unsubscribeHandle = videoHandle.subscribe(() => {
    handleChanges += 1;
  });
  videoHandle.setSource(capture);
  let wrongLabelRejected = false;
  try {
    handleAgent.setState({
      connected: false,
      publications: [{ slot: "v0", label: "screen", active: true }],
    });
  } catch (error) {
    wrongLabelRejected = error instanceof TypeError;
  }
  handleAgent.disconnect();
  videoHandle.setSource(null);
  videoHandle.setSource(capture);
  const sourceRetained = videoHandle.source === capture;
  unsubscribeHandle();
  handleAgent.close();
  let closedHandleRejected = false;
  try {
    videoHandle.setSource(capture);
  } catch (error) {
    closedHandleRejected = error.message === "agent is closed";
  }
  const dual = window.pulsebeam.createAgent({
    endpoint: location.origin,
    token: "dual-handle-token",
    topology: { localVideos: 2 },
  });
  const dualCamera = dual.local.video("camera");
  const dualScreen = dual.local.video("screen");
  dual.connect();
  dual.disconnect();
  dualScreen.setSource(capture);
  dualScreen.setSource(null);
  let exhaustedAfterClearing;
  try {
    dual.local.video("aux");
  } catch (error) {
    exhaustedAfterClearing = error;
  }
  let invalidLabelRejected = false;
  try {
    dual.local.video("ü".repeat(33));
  } catch (error) {
    invalidLabelRejected = error instanceof TypeError;
  }
  const retainedBindings =
    dual.local.video("screen") === dualScreen &&
    dual.local.video("camera") === dualCamera &&
    exhaustedAfterClearing?.kind === "video" &&
    exhaustedAfterClearing.label === "aux" &&
    exhaustedAfterClearing.capacity === 2 &&
    invalidLabelRejected;
  dual.close();
  const localHandles =
    retainedBindings &&
    videoHandle === sameVideoHandle &&
    audioHandle.kind === "audio" &&
    audioHandle.label === "camera" &&
    capacity?.name === "LocalTrackCapacityError" &&
    capacity.kind === "video" &&
    capacity.label === "screen" &&
    capacity.capacity === 1 &&
    wrongLabelRejected &&
    handleChanges === 3 &&
    sourceRetained &&
    videoHandle.source === null &&
    closedHandleRejected &&
    track.readyState === "live";
  await first.replaceLocalTrack("v0", track, {
    contentHint: "motion",
    encodings: [],
  });
  const latestOnly = first.getSnapshot().connection === "disconnected";
  await first.setLocalMuted("v0", true);
  const muted = !track.enabled;
  await first.setLocalMuted("v0", false);
  const unmuted = track.enabled;
  await first.replaceLocalTrack("v0", null, {
    contentHint: "motion",
  });

  const events = [];
  const removeEvents = first.subscribeEvents((event) => events.push(event));
  let invalidStateRejected = false;
  try {
    first.setState({
      connected: false,
      video: [
        {
          slot: -1,
          trackId: "invalid-slot",
          height: 720,
          minHeight: 180,
          minFps: 15,
          priority: 100,
        },
      ],
    });
  } catch (error) {
    invalidStateRejected = /invalid desired state/.test(error.message);
  }
  const serializationFailureNonterminal =
    invalidStateRejected &&
    first.getSnapshot().connection !== "terminal-failure" &&
    first.getSnapshot().failure === null;
  first.setState({
    connected: false,
    video: [
      {
        slot: 0,
        trackId: "duplicate-a",
        height: 720,
        minHeight: 180,
        minFps: 15,
        priority: 100,
      },
      {
        slot: 0,
        trackId: "duplicate-b",
        height: 720,
        minHeight: 180,
        minFps: 15,
        priority: 100,
      },
    ],
  });
  await new Promise((resolve) => setTimeout(resolve, 20));
  const warningsBeforeSilent = warnings.length;
  first.setState({
    connected: false,
    topics: [{ name: "presence", mode: "latest", publish: true }],
  });
  const validationRejected = await first.setLocalMuted("v0", true).then(
    () => false,
    () => true,
  );
  const silentValidationRejected = await silent
    .setLocalMuted("missing", true)
    .then(
      () => false,
      () => true,
    );
  silent.setState({
    connected: false,
    video: [
      {
        slot: 0,
        trackId: "silent-duplicate-a",
        height: 720,
        minHeight: 180,
        minFps: 15,
        priority: 100,
      },
      {
        slot: 0,
        trackId: "silent-duplicate-b",
        height: 720,
        minHeight: 180,
        minFps: 15,
        priority: 100,
      },
    ],
  });
  await new Promise((resolve) => setTimeout(resolve, 20));
  const scopedLogging =
    warningsBeforeSilent > 0 && warnings.length === warningsBeforeSilent;
  first.sendTopic("presence", "latest", new Uint8Array([1, 2, 3]));
  await new Promise((resolve) => setTimeout(resolve, 20));
  removeEvents();
  removeEvents();

  let calls = 0;
  const remove = first.subscribe(() => {
    calls += 1;
  });
  remove();
  remove();
  first.close();
  silent.close();
  const closed = first.getSnapshot();
  first.close();
  first.setState({ connected: true });
  console.warn = consoleWarn;

  return {
    exports,
    independent: first !== second,
    configCopied: endpointReads === 1 && tokenReads === 1,
    initialStable,
    initialFrozen,
    initial: initial.connection,
    latestOnly:
      latestOnly &&
      events.some(
        (event) =>
          event.type === "topic-send-dropped" &&
          event.topic === "presence" &&
          event.reason !== "not-registered",
      ),
    closeBeforeSettlement: second.getSnapshot().connection === "disconnected",
    localOperations: muted && unmuted,
    localHandles,
    validationRejected,
    scopedLogging: silentValidationRejected && scopedLogging,
    serializationFailureNonterminal,
    failureEvent: events.some(
      (event) => event.type === "failure" && event.class === "validation",
    ),
    coreValidationEvent: events.some(
      (event) =>
        event.type === "failure" &&
        event.class === "validation" &&
        event.message.startsWith("core rejected input:"),
    ),
    callerOwnsTrack: track.readyState === "live",
    noRemovedListenerCalls: calls === 0,
    closed: closed.connection,
    postClose: first.getSnapshot() === closed,
  };
})();
