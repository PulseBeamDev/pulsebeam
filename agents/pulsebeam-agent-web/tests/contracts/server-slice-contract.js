(async () => {
  const waitForSnapshot = (agent, predicate, label) => new Promise((resolve, reject) => {
    if (predicate(agent.getSnapshot())) {
      resolve(agent.getSnapshot());
      return;
    }
    const deadline = setTimeout(() => {
      remove();
      reject(new Error(`${label}: ${JSON.stringify(agent.getSnapshot(), bigintJson)}`));
    }, 15000);
    const remove = agent.subscribe(() => {
      const snapshot = agent.getSnapshot();
      if (!predicate(snapshot)) return;
      clearTimeout(deadline);
      remove();
      resolve(snapshot);
    });
  });
  const bigintJson = (_key, value) => typeof value === "bigint" ? value.toString() : value;
  const roomId = `web-${crypto.randomUUID().slice(0, 12)}`;
  const config = {
    endpoint: "http://127.0.0.1:7070",
    roomId,
    topology: { localVideo: ["camera"], localAudio: ["microphone"], remoteVideo: 1, remoteAudio: 1 },
  };
  const first = await window.pulsebeam.createAgent(config);
  const second = await window.pulsebeam.createAgent(config);
  const firstCursor = first.openTopic({ name: "cursor", mode: "latest", publish: true });
  const firstChat = first.openTopic({ name: "chat", mode: "ordered", publish: true });
  const secondCursor = second.openTopic({ name: "cursor", mode: "latest", subscribe: true });
  const secondChat = second.openTopic({ name: "chat", mode: "ordered", subscribe: true });
  const messages = [];
  let resynchronizations = 0;
  secondCursor.subscribe((delivery) => {
    if (delivery.type === "message") messages.push({ mode: delivery.mode, text: new TextDecoder().decode(delivery.payload) });
  });
  secondChat.subscribe((delivery) => {
    if (delivery.type === "message") {
      messages.push({ mode: delivery.mode, text: new TextDecoder().decode(delivery.payload) });
    } else {
      resynchronizations += 1;
    }
  });
  const received = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("topic messages timed out")), 15000);
    const check = () => {
      if (messages.length !== 2) return;
      clearTimeout(deadline);
      resolve();
    };
    secondCursor.subscribe(check);
    secondChat.subscribe(check);
  });

  const firstCanvas = document.createElement("canvas");
  const secondCanvas = document.createElement("canvas");
  firstCanvas.width = secondCanvas.width = 64;
  firstCanvas.height = secondCanvas.height = 64;
  const firstVideo = firstCanvas.captureStream(5).getVideoTracks()[0];
  const secondVideo = secondCanvas.captureStream(5).getVideoTracks()[0];
  const firstAudioContext = new AudioContext();
  const secondAudioContext = new AudioContext();
  const firstAudio = firstAudioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  const secondAudio = secondAudioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  await first.setLocalTrack("camera", firstVideo, "motion");
  await first.setLocalTrack("microphone", firstAudio, "speech");
  await second.setLocalTrack("camera", secondVideo, "detail");
  await second.setLocalTrack("microphone", secondAudio, "music");
  second.connect();
  await waitForSnapshot(second, (snapshot) => snapshot.connection === "connected", "second connect");
  first.connect();
  await waitForSnapshot(first, (snapshot) => snapshot.connection === "connected", "first connect");
  const secondDiscovered = await waitForSnapshot(
    second,
    (snapshot) => snapshot.publications.some((publication) => publication.kind === "video")
      && snapshot.publications.some((publication) => publication.kind === "audio"),
    "second publication discovery",
  );
  const firstDiscovered = await waitForSnapshot(
    first,
    (snapshot) => snapshot.publications.some((publication) => publication.kind === "video")
      && snapshot.publications.some((publication) => publication.kind === "audio"),
    "first publication discovery",
  );
  const firstPublicationId = secondDiscovered.publications.find((publication) => publication.kind === "video").id;
  const secondPublicationId = firstDiscovered.publications.find((publication) => publication.kind === "video").id;
  const firstAudioId = secondDiscovered.publications.find((publication) => publication.kind === "audio").id;
  second.setVideoDemand([{ slot: 0, publicationId: firstPublicationId, height: 360, minHeight: 180, minFps: 7, priority: 2 }]);
  first.setVideoDemand([{ slot: 0, publicationId: secondPublicationId, height: 540, minHeight: 180, minFps: 7, priority: 1 }]);
  second.setAudioDemand({ pinned: [firstAudioId], automatic: false });
  first.setAudioDemand({ automatic: true });
  first.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  second.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  const secondBound = await waitForSnapshot(
    second,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === firstPublicationId && video.bound && video.track)
      && snapshot.remoteAudio.some((audio) => audio.bound && audio.track),
    "second media binding",
  );
  await waitForSnapshot(
    first,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === secondPublicationId && video.bound && video.track)
      && snapshot.remoteAudio.some((audio) => audio.bound && audio.track),
    "first media binding",
  );
  const before = secondBound.remoteVideo.find((video) => video.publicationId === firstPublicationId);

  firstCursor.send(new TextEncoder().encode("latest"));
  firstChat.send(new TextEncoder().encode("ordered"));
  await received;
  const participantId = first.getSnapshot().participantId;
  const generation = first.getSnapshot().generation;
  first.reconnect();
  const replacement = await waitForSnapshot(
    first,
    (snapshot) => snapshot.connection === "connected" && snapshot.generation !== generation,
    "transport replacement",
  );
  const rebound = await waitForSnapshot(
    second,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === firstPublicationId && video.bound && video.track),
    "video rebound",
  );
  const after = rebound.remoteVideo.find((video) => video.publicationId === firstPublicationId);
  const recovered = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("ordered recovery timed out")), 15000);
    const remove = secondChat.subscribe((delivery) => {
      if (delivery.type !== "message" || new TextDecoder().decode(delivery.payload) !== "recovered") return;
      clearTimeout(deadline);
      remove();
      resolve();
    });
  });
  firstChat.send(new TextEncoder().encode("recovered"));
  await recovered;
  const statistics = await first.statistics();
  await first.close();
  await second.close();
  firstVideo.stop();
  secondVideo.stop();
  firstAudio.stop();
  secondAudio.stop();
  await firstAudioContext.close();
  await secondAudioContext.close();
  return {
    messages: messages.sort((left, right) => left.text.localeCompare(right.text)),
    resynchronizations,
    participantId,
    replacementParticipantId: replacement.participantId,
    stableStream: before.stream === after.stream,
    senderCount: statistics.senders.length,
    firstConnection: first.getSnapshot().connection,
    secondConnection: second.getSnapshot().connection,
  };
})()
