(async () => {
  const endpoint = "http://127.0.0.1:7070";
  const topology = {
    localVideo: ["camera"],
    remoteVideo: 1,
    remoteAudio: 0,
  };
  const sender = window.pulsebeam.createAgent({
    endpoint,
    roomId: "public-web-contract",
    topology,
  });
  const receiver = window.pulsebeam.createAgent({
    endpoint,
    roomId: "public-web-contract",
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
  await sender.replaceLocalTrack("camera", localTrack, {
    contentHint: "motion",
  });
  sender.setState({
    connected: true,
    publications: [{ slot: "camera", active: true }],
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
  const discovered = await waitFor(
    receiver,
    (snapshot) =>
      snapshot.publications.some(
        (publication) =>
          publication.kind === "video" &&
          publication.participantId === senderConnected.participantId,
      ),
    "remote publication",
  );
  const publication = discovered.publications.find(
    (candidate) =>
      candidate.kind === "video" &&
      candidate.participantId === senderConnected.participantId,
  );
  receiver.setState({
    connected: true,
    video: [
      {
        slot: 0,
        trackId: publication.id,
        height: 180,
        minHeight: 1,
        minFps: 1,
        priority: 100,
      },
    ],
  });
  const delivered = await waitFor(
    receiver,
    (snapshot) => snapshot.tracks[publication.id]?.media.readyState === "live",
    "remote media",
  );

  const previousGeneration = delivered.generation;
  receiver.reconnect();
  const reconnected = await waitFor(
    receiver,
    (snapshot) =>
      snapshot.connection === "connected" &&
      snapshot.generation !== previousGeneration &&
      snapshot.tracks[publication.id]?.media.readyState === "live",
    "reconnected media",
  );

  const runtimeEvents = [];
  const removeRuntimeEvents = sender.subscribeEvents((event) =>
    runtimeEvents.push(event),
  );
  const originalSetParameters = RTCRtpSender.prototype.setParameters;
  RTCRtpSender.prototype.setParameters = () =>
    Promise.reject(new DOMException("contract runtime failure"));
  const runtimeRejected = await sender
    .replaceLocalTrack("camera", localTrack, { contentHint: "motion" })
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

  sender.close();
  receiver.close();
  const callerOwnsTrack = localTrack.readyState === "live";
  localTrack.stop();
  return {
    connected:
      senderConnected.participantId !== null &&
      receiverConnected.participantId !== null,
    discovered: publication.id.length > 0,
    delivered: delivered.tracks[publication.id].kind === "video",
    reconnected: reconnected.tracks[publication.id].kind === "video",
    topicMetadata:
      receivedTopic.publisherId === senderConnected.participantId &&
      receivedTopic.streamId > 0 &&
      receivedTopic.sequence >= 0 &&
      receivedTopic.payload.join(",") === "7,8,9",
    runtimeFailureEvent,
    callerOwnsTrack,
  };
})()
