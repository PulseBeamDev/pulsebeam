(async () => {
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

  await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error("latest desired state was not applied")),
      5000,
    );
    const inspect = () => {
      if (first.getSnapshot().desiredRevision !== 1) return;
      clearTimeout(timeout);
      remove();
      resolve();
    };
    const remove = first.subscribe(inspect);
    inspect();
  });
  const latestOnly =
    first.getSnapshot().desiredRevision === 1 &&
    first.getSnapshot().connection === "disconnected";

  const canvas = document.createElement("canvas");
  const track = canvas.captureStream(1).getVideoTracks()[0];
  await first.replaceLocalTrack("camera", track, {
    contentHint: "motion",
    encodings: [],
  });
  await first.setLocalMuted("camera", true);
  const muted = !track.enabled;
  await first.setLocalMuted("camera", false);
  const unmuted = track.enabled;
  await first.replaceLocalTrack("camera", null, {
    contentHint: "motion",
  });

  const events = [];
  const removeEvents = first.subscribeEvents((event) => events.push(event));
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
    latestOnly,
    closeBeforeSettlement: second.getSnapshot().connection === "disconnected",
    localOperations: muted && unmuted,
    validationRejected,
    failureEvent: events.some(
      (event) => event.type === "failure" && event.class === "validation",
    ),
    callerOwnsTrack: track.readyState === "live",
    noRemovedListenerCalls: calls === 0,
    closed: closed.connection,
    postClose: first.getSnapshot() === closed,
  };
})();
