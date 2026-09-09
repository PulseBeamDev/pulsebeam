globalThis.__pulsebeamPublic = (async () => {
  const exports = Object.keys(window.pulsebeam).sort();
  let endpointReads = 0;
  let roomReads = 0;
  const config = {
    get endpoint() {
      endpointReads += 1;
      return location.origin;
    },
    get roomId() {
      roomReads += 1;
      return "contract";
    },
    requestHeaders: { "x-contract": "public-agent" },
    topology: {
      localVideo: ["camera"],
      localAudio: ["microphone"],
      remoteVideo: 1,
      remoteAudio: 1,
    },
  };
  const first = window.pulsebeam.createAgent(config);
  const second = window.pulsebeam.createAgent({
    endpoint: location.origin,
    roomId: "closed",
    topology: {},
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
  const publications = [{ slot: "camera", active: true }];
  first.setState({ connected: true, publications });
  publications.push({ slot: "mutated", active: true });
  first.setState({
    connected: false,
    topics: [{ name: "presence", mode: "latest", publish: true }],
  });

  const canvas = document.createElement("canvas");
  const track = canvas.captureStream(1).getVideoTracks()[0];
  await first.replaceLocalTrack("camera", track, {
    contentHint: "motion",
    encodings: [],
  });
  const latestOnly = first.getSnapshot().connection === "disconnected";
  await first.setLocalMuted("camera", true);
  const muted = !track.enabled;
  await first.setLocalMuted("camera", false);
  const unmuted = track.enabled;
  await first.replaceLocalTrack("camera", null, {
    contentHint: "motion",
  });

  const events = [];
  const removeEvents = first.subscribeEvents((event) => events.push(event));
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
  const serializationFailureNonterminal =
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
  first.setState({
    connected: false,
    topics: [{ name: "presence", mode: "latest", publish: true }],
  });
  const validationRejected = await first.setLocalMuted("camera", true).then(
    () => false,
    () => true,
  );
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
  const closed = first.getSnapshot();
  first.close();
  first.setState({ connected: true });

  return {
    exports,
    independent: first !== second,
    configCopied: endpointReads === 1 && roomReads === 1,
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
    validationRejected,
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
