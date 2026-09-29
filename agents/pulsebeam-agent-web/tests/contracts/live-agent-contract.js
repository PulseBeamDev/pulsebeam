(async () => {
  const endpoint = "http://127.0.0.1:7070";
  // Test-only observation of the browser's real receivers, without a public Agent seam.
  const peers = new Set();
  const originalAddTransceiver = RTCPeerConnection.prototype.addTransceiver;
  RTCPeerConnection.prototype.addTransceiver = function (...args) {
    peers.add(this);
    return originalAddTransceiver.apply(this, args);
  };
  const relays = new WeakMap();
  const relayFailures = [];
  const originalSetRemoteDescription =
    RTCPeerConnection.prototype.setRemoteDescription;
  RTCPeerConnection.prototype.setRemoteDescription = async function (
    description,
  ) {
    try {
      // Remove TCP alternatives so ICE cannot bypass the UDP observation path.
      const lines = description.sdp
        .split(/\r?\n/)
        .filter(
          (line) =>
            !line.startsWith("a=candidate:") ||
            line.split(/\s+/)[2]?.toLowerCase() !== "tcp",
        );
      const candidates = lines.filter((line) =>
        line.startsWith("a=candidate:"),
      );
      if (
        description.type !== "answer" ||
        candidates.length === 0 ||
        !lines.includes("a=ice-lite") ||
        relays.has(this)
      ) {
        throw new Error(
          "packet observation requires one candidate-bearing answer per peer",
        );
      }
      const destinations = candidates
        .map((line) => {
          const fields = line.split(/\s+/);
          if (
            fields[1] !== "1" ||
            fields[2].toLowerCase() !== "udp" ||
            !/^(?:\d+\.\d+\.\d+\.\d+|[a-f0-9]*:[a-f0-9:]+)$/i.test(fields[4]) ||
            fields[7] !== "host"
          ) {
            throw new Error(`unsupported relay candidate: ${line}`);
          }
          return fields[4].includes(":") ? null : `${fields[4]}:${fields[5]}`;
        })
        .filter(Boolean);
      const ids = new Set(
        lines.flatMap((line) => {
          const match = line.match(
            /^a=extmap:(\d+)(?:\/\w+)? http:\/\/www.webrtc.org\/experiments\/rtp-hdrext\/playout-delay$/,
          );
          return match ? [Number(match[1])] : [];
        }),
      );
      if (ids.size !== 1)
        throw new Error(`ambiguous negotiated playout extension: ${[...ids]}`);
      const extension = [...ids][0];
      const destination =
        destinations.find((value) => value.startsWith("127.0.0.1:")) ??
        destinations[0];
      if (!destination)
        throw new Error("packet relay requires an IPv4 server candidate");
      // Rewrite every route, including IPv6 candidates, to the same local relay.
      const response = await fetch(
        `/__test/rtp-relay?destination=${destination}&extension=${extension}`,
      );
      if (!response.ok) throw new Error(await response.text());
      const relay = await response.json();
      relays.set(this, relay);
      const sdp = lines
        .map((line) => {
          if (!line.startsWith("a=candidate:")) return line;
          const fields = line.split(/\s+/);
          fields[4] = relay.address;
          fields[5] = String(relay.port);
          return fields.join(" ");
        })
        .join("\r\n");
      return await originalSetRemoteDescription.call(this, {
        type: description.type,
        sdp,
      });
    } catch (error) {
      relayFailures.push(String(error));
      throw error;
    }
  };
  const packetEvidence = async (peer, receiver) => {
    const relay = relays.get(peer);
    if (!relay) throw new Error("receiver has no packet relay");
    const stats = await receiver.getStats();
    const inbound = [...stats.values()].filter(
      (entry) => entry.type === "inbound-rtp" && entry.kind === "video",
    );
    if (inbound.length !== 1)
      throw new Error(`expected one inbound video SSRC, got ${inbound.length}`);
    for (let attempt = 0; attempt < 100; attempt++) {
      const response = await fetch(
        `/__test/rtp-relay/${relay.id}/${inbound[0].ssrc}`,
      );
      if (!response.ok) throw new Error(await response.text());
      const observation = await response.json();
      if (observation.packets >= 4)
        return { relay: relay.id, ssrc: inbound[0].ssrc, ...observation };
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(
      `no captured RTP for relay ${relay.id}, SSRC ${inbound[0].ssrc}`,
    );
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

  const secondReceiver = window.pulsebeam.createAgent({
    endpoint,
    token: "__SECOND_RECEIVER_TOKEN__",
    topology,
  });

  const waitFor = (agent, predicate, label) =>
    new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        remove();
        reject(
          new Error(
            `timed out waiting for ${label}: ${JSON.stringify({ relayFailures, failure: agent.getSnapshot().failure })}`,
          ),
        );
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
      snapshot.mapping.video.some(
        (entry) => entry.publicationId === publication.id,
      ) &&
      snapshot.tracks[publication.id]?.media.readyState === "live",
    "fixed-policy remote media",
  );

  const previousGeneration = delivered.generation;
  const oldTrack = delivered.tracks[publication.id].media;
  const oldPeer = peerForTrack(oldTrack);
  const oldReceiver = oldPeer
    ?.getReceivers()
    .find((entry) => entry.track === oldTrack);
  let staleOnTrack = 0;
  const countStaleOnTrack = () => {
    staleOnTrack += 1;
  };
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
  for (
    let attempt = 0;
    attempt < 100 && fixedPackets < initialPackets + 4;
    attempt++
  ) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    fixedPackets = await receivedPackets();
  }
  if (fixedPackets < initialPackets + 4) {
    throw new Error(
      `no continuing fixed-policy RTP: initial=${initialPackets} final=${fixedPackets}`,
    );
  }
  const decodedFrames = async () => {
    const stats = await oldReceiver.getStats();
    return [...stats.values()]
      .filter(
        (report) => report.type === "inbound-rtp" && report.kind === "video",
      )
      .reduce((sum, report) => sum + (report.framesDecoded ?? 0), 0);
  };
  const initialFrames = await decodedFrames();
  let fixedFrames = initialFrames;
  for (
    let attempt = 0;
    attempt < 100 && fixedFrames < initialFrames + 2;
    attempt++
  ) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    fixedFrames = await decodedFrames();
  }
  if (fixedFrames < initialFrames + 2) {
    throw new Error(
      `no decoded fixed-policy frames: initial=${initialFrames} final=${fixedFrames}`,
    );
  }
  const fixedRtp = await packetEvidence(oldPeer, oldReceiver);
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
      snapshot.mapping.video.some(
        (entry) => entry.publicationId === publication.id,
      ) &&
      snapshot.tracks[publication.id]?.media.readyState === "live" &&
      oldTrack.readyState === "ended",
    "fresh default receiver after fixed playout",
  ).catch((error) => {
    const snapshot = receiver.getSnapshot();
    throw new Error(
      `${error.message}: ${JSON.stringify({
        generation: String(snapshot.generation),
        oldGeneration: String(previousGeneration),
        acceptedRevision: String(snapshot.mapping.acceptedIntentRevision),
        desiredRevision: String(snapshot.desiredRevision),
        failure: snapshot.failure,
        events: receiverEvents,
        oldState: oldTrack.readyState,
        oldPeerState: oldPeer?.signalingState,
      })}`,
    );
  });
  removeReceiverEvents();
  const newTrack = reconnected.tracks[publication.id].media;
  const newPeer = peerForTrack(newTrack);
  const newReceiver = newPeer
    ?.getReceivers()
    .find((entry) => entry.track === newTrack);
  const restoredStats = async () => {
    if (!newReceiver) return { packets: 0, frames: 0 };
    const stats = await newReceiver.getStats();
    return [...stats.values()]
      .filter(
        (report) => report.type === "inbound-rtp" && report.kind === "video",
      )
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
  for (
    let attempt = 0;
    attempt < 100 &&
    (restored.packets < restoredInitial.packets + 4 ||
      restored.frames < restoredInitial.frames + 2);
    attempt++
  ) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    restored = await restoredStats();
  }
  const defaultRtp = await packetEvidence(newPeer, newReceiver);
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
  RTCPeerConnection.prototype.setRemoteDescription =
    originalSetRemoteDescription;

  secondReceiver.setState({ connected: true });
  const secondConnected = await waitFor(
    secondReceiver,
    (snapshot) =>
      snapshot.connection === "connected" &&
      snapshot.catalog.publications.some(
        (entry) => entry.id === publication.id,
      ),
    "independent receiver discovery",
  );
  const mount = (agent) => {
    const handle = agent.remoteVideoTracks.find(
      (entry) => entry.publicationId === publication.id,
    );
    if (!handle) throw new Error("missing remote video handle");
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.style.cssText = "width:160px;height:120px;display:inline-block";
    document.body.append(video);
    return {
      handle,
      video,
      attachment: window.pulsebeam.attachRemoteVideo(handle, video),
    };
  };
  const rendered = (video) =>
    new Promise((resolve, reject) => {
      let count = 0;
      let callback;
      const timeout = setTimeout(() => {
        video.cancelVideoFrameCallback(callback);
        reject(new Error("timed out waiting for rendered remote frames"));
      }, 20000);
      const observe = () => {
        count += 1;
        if (count >= 2) {
          clearTimeout(timeout);
          resolve();
        } else {
          callback = video.requestVideoFrameCallback(observe);
        }
      };
      callback = video.requestVideoFrameCallback(observe);
    });
  const firstView = mount(receiver);
  const secondView = mount(secondReceiver);
  await Promise.all([rendered(firstView.video), rendered(secondView.video)]);
  const independentRendering =
    receiver.getSnapshot().connection === "connected" &&
    secondReceiver.getSnapshot().connection === "connected" &&
    receiverConnected.participantId !== secondConnected.participantId &&
    firstView.video.srcObject.getVideoTracks()[0] ===
      receiver.getSnapshot().tracks[publication.id].media &&
    secondView.video.srcObject.getVideoTracks()[0] ===
      secondReceiver.getSnapshot().tracks[publication.id].media &&
    firstView.video.srcObject.getVideoTracks()[0] !==
      secondView.video.srcObject.getVideoTracks()[0];
  firstView.attachment.close();
  await rendered(secondView.video);
  const independentDetach =
    firstView.video.srcObject === null &&
    secondReceiver.getSnapshot().connection === "connected";
  firstView.attachment = window.pulsebeam.attachRemoteVideo(
    firstView.handle,
    firstView.video,
  );
  await rendered(firstView.video);

  sender.setState({ connected: true, publications: [] });
  const removed = (snapshot) =>
    !snapshot.catalog.publications.some(
      (entry) => entry.id === publication.id,
    ) &&
    !snapshot.mapping.video.some(
      (entry) => entry.publicationId === publication.id,
    ) &&
    snapshot.tracks[publication.id] === undefined;
  await Promise.all([
    waitFor(receiver, removed, "first mounted publication removal"),
    waitFor(secondReceiver, removed, "second mounted publication removal"),
  ]);
  const revisions = [
    receiver.getSnapshot().desiredRevision,
    secondReceiver.getSnapshot().desiredRevision,
  ];
  firstView.handle.setReceiveOptions({ minHeight: 720 });
  secondView.handle.setReceiveOptions({ priority: 100 });
  const stale = window.pulsebeam.attachRemoteVideo(
    firstView.handle,
    firstView.video,
  );
  const mountedRemoval =
    !firstView.handle.active &&
    !secondView.handle.active &&
    (firstView.video.srcObject?.getTracks().length ?? 0) === 0 &&
    (secondView.video.srcObject?.getTracks().length ?? 0) === 0 &&
    !receiver.remoteVideoTracks.some(
      (entry) => entry.publicationId === publication.id,
    ) &&
    !secondReceiver.remoteVideoTracks.some(
      (entry) => entry.publicationId === publication.id,
    ) &&
    receiver.getSnapshot().desiredRevision === revisions[0] &&
    secondReceiver.getSnapshot().desiredRevision === revisions[1];
  stale.close();
  firstView.attachment.close();
  secondView.attachment.close();
  firstView.video.remove();
  secondView.video.remove();
  secondReceiver.close();

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
    independentRendering: independentRendering && independentDetach,
    mountedRemoval,
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
    fixedRtp,
    defaultRtp,
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
