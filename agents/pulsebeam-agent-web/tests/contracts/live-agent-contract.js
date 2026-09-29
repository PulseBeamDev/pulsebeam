(async () => {
  const endpoint = "http://127.0.0.1:7070";
  // Test-only observation of the browser's real receivers, without a public Agent seam.
  const peers = new Set();
  const originalAddTransceiver = RTCPeerConnection.prototype.addTransceiver;
  RTCPeerConnection.prototype.addTransceiver = function (...args) {
    peers.add(this);
    return originalAddTransceiver.apply(this, args);
  };
  const peerForTrack = (track) =>
    [...peers].find((peer) =>
      peer.getReceivers().some((receiver) => receiver.track === track),
    );
  const topology = {
    localVideos: 1,
    localAudios: 0,
    remoteVideos: 1,
    remoteAudios: 0,
  };
  const sender = window.pulsebeam.createAgent({
    endpoint,
    token: "__SENDER_TOKEN__",
    topology,
  });
  const receiver = window.pulsebeam.createAgent({
    endpoint,
    token: "__RECEIVER_TOKEN__",
    topology,
  });

  const waitFor = (agent, predicate, label) =>
    new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        remove();
        reject(new Error(`timed out waiting for ${label}`));
      }, 20000);
      const inspect = () => {
        const snapshot = agent.getSnapshot();
        if (!predicate(snapshot)) return;
        clearTimeout(timeout);
        remove();
        resolve(snapshot);
      };
      const remove = agent.subscribe(inspect);
      inspect();
    });

  const canvas = document.createElement("canvas");
  canvas.width = 16;
  canvas.height = 16;
  const context = canvas.getContext("2d");
  context.fillStyle = "#20a0ff";
  context.fillRect(0, 0, 16, 16);
  const localTrack = canvas.captureStream(5).getVideoTracks()[0];
  const painting = setInterval(() => {
    context.fillStyle = context.fillStyle === "#20a0ff" ? "#ff8020" : "#20a0ff";
    context.fillRect(0, 0, 16, 16);
  }, 200);
  await sender.replaceLocalTrack("v0", localTrack, {
    contentHint: "motion",
  });
  sender.setState({
    connected: true,
    publications: [{ slot: "v0", label: "camera", active: true }],
    topics: [{ name: "chat", mode: "ordered", publish: true }],
  });
  receiver.setState({
    connected: true,
    topics: [{ name: "chat", mode: "ordered", subscribe: true }],
  });

  const senderConnected = await waitFor(
    sender,
    (snapshot) => snapshot.connection === "connected",
    "sender connection",
  );
  const receiverConnected = await waitFor(
    receiver,
    (snapshot) => snapshot.connection === "connected",
    "receiver connection",
  );
  await waitFor(
    sender,
    (snapshot) => snapshot.topics.publishers[0]?.connected === true,
    "topic publisher",
  );
  await waitFor(
    receiver,
    (snapshot) => snapshot.topics.subscribers[0]?.connected === true,
    "topic subscriber",
  );
  const discovered = await waitFor(
    receiver,
    (snapshot) =>
      snapshot.catalog.publications.some(
        (publication) =>
          publication.kind === "video" &&
          publication.participantId === senderConnected.participantId,
      ),
    "remote publication",
  );
  const publication = discovered.catalog.publications.find(
    (candidate) =>
      candidate.kind === "video" &&
      candidate.participantId === senderConnected.participantId,
  );
  const topicMessage = new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      remove();
      reject(new Error("timed out waiting for ordered topic message"));
    }, 20000);
    const remove = receiver.subscribeEvents((event) => {
      if (event.type !== "topic-message" || event.mode !== "ordered") return;
      clearTimeout(timeout);
      remove();
      resolve(event);
    });
  });
  sender.sendTopic("chat", "ordered", new Uint8Array([7, 8, 9]));
  const receivedTopic = await topicMessage;
  receiver.setState({
    connected: true,
    video: [
      {
        slot: 0,
        selector: { participantExternalId: "web-sender", label: "camera" },
        height: 180,
        minHeight: 1,
        minFps: 1,
        priority: 100,
        playoutDelay: { mode: "fixed", minMs: 100, maxMs: 100 },
      },
    ],
  });
  const delivered = await waitFor(
    receiver,
    (snapshot) =>
      snapshot.mapping.acceptedIntentRevision > 0 &&
      snapshot.mapping.video.some((entry) => entry.publicationId === publication.id) &&
      snapshot.tracks[publication.id]?.media.readyState === "live",
    "fixed-policy remote media",
  );

  const previousGeneration = delivered.generation;
  const oldTrack = delivered.tracks[publication.id].media;
  const oldPeer = peerForTrack(oldTrack);
  const oldReceiver = oldPeer?.getReceivers().find((entry) => entry.track === oldTrack);
  let staleOnTrack = 0;
  const countStaleOnTrack = () => { staleOnTrack += 1; };
  oldPeer?.addEventListener("track", countStaleOnTrack);
  const fixedRevision = delivered.mapping.acceptedIntentRevision;
  const receivedPackets = async () => {
    const stats = await oldReceiver.getStats();
    let count = 0;
    for (const report of stats.values()) {
      if (report.type === "inbound-rtp" && report.kind === "video") {
        count += report.packetsReceived ?? 0;
      }
    }
    return count;
  };
  const initialPackets = await receivedPackets();
  let fixedPackets = initialPackets;
  for (let attempt = 0; attempt < 100 && fixedPackets < initialPackets + 4; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    fixedPackets = await receivedPackets();
  }
  if (fixedPackets < initialPackets + 4) {
    throw new Error(`no continuing fixed-policy RTP: initial=${initialPackets} final=${fixedPackets}`);
  }
  const decodedFrames = async () => {
    const stats = await oldReceiver.getStats();
    return [...stats.values()]
      .filter((report) => report.type === "inbound-rtp" && report.kind === "video")
      .reduce((sum, report) => sum + (report.framesDecoded ?? 0), 0);
  };
  const initialFrames = await decodedFrames();
  let fixedFrames = initialFrames;
  for (let attempt = 0; attempt < 100 && fixedFrames < initialFrames + 2; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    fixedFrames = await decodedFrames();
  }
  if (fixedFrames < initialFrames + 2) {
    throw new Error(`no decoded fixed-policy frames: initial=${initialFrames} final=${fixedFrames}`);
  }
  const receiverEvents = [];
  const removeReceiverEvents = receiver.subscribeEvents((event) => {
    if (event.type !== "topic-message") receiverEvents.push(event.type);
  });
  receiver.setState({
    connected: true,
    video: [
      {
        slot: 0,
        selector: { participantExternalId: "web-sender", label: "camera" },
        height: 180,
        minHeight: 1,
        minFps: 1,
        priority: 100,
      },
    ],
  });
  const reconnected = await waitFor(
    receiver,
    (snapshot) =>
      snapshot.connection === "connected" &&
      snapshot.generation !== previousGeneration &&
      snapshot.mapping.acceptedIntentRevision > 0 &&
      snapshot.mapping.video.some((entry) => entry.publicationId === publication.id) &&
      snapshot.tracks[publication.id]?.media.readyState === "live" &&
      oldTrack.readyState === "ended",
    "fresh default receiver after fixed playout",
  ).catch((error) => {
    const snapshot = receiver.getSnapshot();
    throw new Error(`${error.message}: ${JSON.stringify({
      generation: String(snapshot.generation),
      oldGeneration: String(previousGeneration),
      acceptedRevision: String(snapshot.mapping.acceptedIntentRevision),
      desiredRevision: String(snapshot.desiredRevision),
      failure: snapshot.failure,
      events: receiverEvents,
      oldState: oldTrack.readyState,
      oldPeerState: oldPeer?.signalingState,
    })}`);
  });
  removeReceiverEvents();
  const newTrack = reconnected.tracks[publication.id].media;
  const newPeer = peerForTrack(newTrack);
  const newReceiver = newPeer?.getReceivers().find((entry) => entry.track === newTrack);
  const restoredStats = async () => {
    if (!newReceiver) return { packets: 0, frames: 0 };
    const stats = await newReceiver.getStats();
    return [...stats.values()]
      .filter((report) => report.type === "inbound-rtp" && report.kind === "video")
      .reduce(
        (sum, report) => ({
          packets: sum.packets + (report.packetsReceived ?? 0),
          frames: sum.frames + (report.framesDecoded ?? 0),
        }),
        { packets: 0, frames: 0 },
      );
  };
  const restoredInitial = await restoredStats();
  let restored = restoredInitial;
  for (let attempt = 0; attempt < 100 &&
      (restored.packets < restoredInitial.packets + 4 ||
       restored.frames < restoredInitial.frames + 2); attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    restored = await restoredStats();
  }
  const defaultRecreated =
    restored.packets >= restoredInitial.packets + 4 &&
    restored.frames >= restoredInitial.frames + 2 &&
    staleOnTrack === 0 &&
    receiver.getSnapshot().tracks[publication.id]?.media === newTrack &&
    fixedRevision > 0 &&
    oldPeer !== undefined &&
    oldPeer.signalingState === "closed" &&
    oldReceiver !== undefined &&
    newPeer !== undefined &&
    newPeer !== oldPeer &&
    newReceiver !== undefined &&
    newReceiver !== oldReceiver &&
    oldTrack.readyState === "ended" &&
    newTrack.readyState === "live";
  oldPeer?.removeEventListener("track", countStaleOnTrack);
  RTCPeerConnection.prototype.addTransceiver = originalAddTransceiver;

  const runtimeEvents = [];
  const removeRuntimeEvents = sender.subscribeEvents((event) =>
    runtimeEvents.push(event),
  );
  const originalSetParameters = RTCRtpSender.prototype.setParameters;
  RTCRtpSender.prototype.setParameters = () =>
    Promise.reject(new DOMException("contract runtime failure"));
  const runtimeRejected = await sender
    .replaceLocalTrack("v0", localTrack, { contentHint: "motion" })
    .then(
      () => false,
      () => true,
    );
  RTCRtpSender.prototype.setParameters = originalSetParameters;
  removeRuntimeEvents();
  const runtimeFailureEvent =
    runtimeRejected &&
    runtimeEvents.some(
      (event) => event.type === "failure" && event.class === "runtime",
    );

  let enterPendingOperation;
  const pendingOperationEntered = new Promise(
    (resolve) => (enterPendingOperation = resolve),
  );
  let releasePendingOperation;
  const pendingOperationGate = new Promise(
    (resolve) => (releasePendingOperation = resolve),
  );
  RTCRtpSender.prototype.setParameters = () => {
    enterPendingOperation();
    return pendingOperationGate;
  };
  const pendingReplacement = sender.replaceLocalTrack("v0", localTrack, {
    contentHint: "motion",
  });
  await pendingOperationEntered;
  sender.close();
  releasePendingOperation();
  const closeDuringLocalOperation = await pendingReplacement.then(
    () => false,
    (error) => error instanceof Error && error.message === "agent is closed",
  );
  RTCRtpSender.prototype.setParameters = originalSetParameters;
  receiver.close();
  const callerOwnsTrack = localTrack.readyState === "live";
  clearInterval(painting);
  localTrack.stop();
  return {
    connected:
      senderConnected.participantId !== null &&
      receiverConnected.participantId !== null,
    discovered:
      publication.id.length > 0 &&
      publication.label === "camera" &&
      discovered.catalog.participants.find(
        (participant) => participant.id === senderConnected.participantId,
      )?.externalId === "web-sender",
    delivered: delivered.tracks[publication.id].kind === "video",
    reconnected: reconnected.tracks[publication.id].kind === "video",
    defaultRecreated,
    topicMetadata:
      receivedTopic.publisherId === senderConnected.participantId &&
      receivedTopic.streamId > 0 &&
      receivedTopic.sequence >= 0 &&
      receivedTopic.payload.join(",") === "7,8,9",
    runtimeFailureEvent,
    closeDuringLocalOperation,
    callerOwnsTrack,
  };
})();
