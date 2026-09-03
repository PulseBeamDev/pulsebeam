(async () => {
  let invalidCode;
  try {
    await window.pulsebeam.createAgent({ endpoint: "relative", roomId: "room", topology: {} });
  } catch (error) {
    invalidCode = error.code;
  }

  const sourceSlots = ["camera"];
  const agent = await window.pulsebeam.createAgent({
    endpoint: window.location.origin,
    roomId: "sender-stats",
    topology: { localVideo: sourceSlots, localAudio: ["microphone"], remoteVideo: 2, remoteAudio: 1 },
  });
  sourceSlots.push("mutated");
  let immutableCode;
  try {
    await agent.setLocalTrack("mutated", null);
  } catch (error) {
    immutableCode = error.code;
  }
  const initialIdentity = agent.getSnapshot() === agent.getSnapshot();
  const revisions = [agent.getSnapshot().desiredRevision];
  const order = [];
  let removeFirst;
  removeFirst = agent.subscribe(() => {
    order.push("first");
    removeFirst();
  });
  const removeSecond = agent.subscribe(() => order.push("second"));

  agent.setVideoDemand([
    { slot: 0, publicationId: "remote/video", height: 720, minHeight: 180, minFps: 15, priority: 4 },
  ]);
  revisions.push(agent.getSnapshot().desiredRevision);
  agent.setAudioDemand({ pinned: ["remote/audio"], automatic: false });
  revisions.push(agent.getSnapshot().desiredRevision);
  agent.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  revisions.push(agent.getSnapshot().desiredRevision);
  removeSecond();

  let commandCode;
  try {
    agent.setVideoDemand([{ slot: 2, publicationId: "outside", height: 360 }]);
  } catch (error) {
    commandCode = error.code;
  }

  const topic = agent.openTopic({ name: "cursor", mode: "latest", publish: true, subscribe: true });
  const topicIdentity = topic === agent.openTopic({
    name: "cursor", mode: "latest", publish: true, subscribe: true,
  });
  revisions.push(agent.getSnapshot().desiredRevision);
  const registeredTopics = agent.getSnapshot().topics.publishers.length;
  const pendingTopic = topic[Symbol.asyncIterator]().next();
  topic.close();
  const topicIteratorClosed = (await pendingTopic).done;
  revisions.push(agent.getSnapshot().desiredRevision);
  const topicsAfterClose = agent.getSnapshot().topics.publishers.length;

  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const track = canvas.captureStream(5).getVideoTracks()[0];
  await agent.setLocalTrack("camera", track, "detail");
  const audioContext = new AudioContext();
  const audioTrack = audioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  await agent.setLocalTrack("microphone", audioTrack, "music");
  revisions.push(agent.getSnapshot().desiredRevision);
  const published = agent.getSnapshot().localPublications.find((item) => item.slot === "camera");
  const publishedAudio = agent.getSnapshot().localPublications.find((item) => item.slot === "microphone");
  await agent.setLocalMuted("camera", true);
  revisions.push(agent.getSnapshot().desiredRevision);
  const muted = agent.getSnapshot().localPublications.find((item) => item.slot === "camera");
  const mutedValue = muted.muted;
  const enabledWhenMuted = muted.track.enabled;
  await agent.setLocalMuted("camera", false);

  const joining = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("agent did not start signaling")), 5000);
    const remove = agent.subscribe(() => {
      if (agent.getSnapshot().connection !== "joining") return;
      clearTimeout(deadline);
      remove();
      resolve();
    });
  });
  agent.connect();
  await joining;
  const statistics = await agent.statistics();
  const sender = statistics.senders.find((item) => item.slot === "camera");
  const audioSender = statistics.senders.find((item) => item.slot === "microphone");
  const replacementCanvas = document.createElement("canvas");
  const replacementTrack = replacementCanvas.captureStream(5).getVideoTracks()[0];
  await agent.setLocalTrack("camera", replacementTrack, "motion");
  const replacementStatistics = await agent.statistics();
  const replacementSender = replacementStatistics.senders.find((item) => item.slot === "camera");
  const snapshotHasReplacement = agent.getSnapshot().localPublications
    .find((item) => item.slot === "camera").track === replacementTrack;
  await agent.setLocalTrack("camera", null);
  await agent.setLocalTrack("microphone", null);
  const removedTracks = agent.getSnapshot().localPublications.filter((item) => item.track).length;
  const closeNotifications = [];
  agent.subscribe(() => closeNotifications.push(agent.getSnapshot().connection));
  await agent.close();
  const closed = agent.getSnapshot();
  const notificationsAtClose = closeNotifications.length;
  agent.subscribe(() => closeNotifications.push("late"));
  track.stop();
  replacementTrack.stop();
  audioTrack.stop();
  await audioContext.close();

  return {
    invalidCode,
    commandCode,
    immutableCode,
    initialIdentity,
    order,
    revisions,
    topicIdentity,
    topicIteratorClosed,
    registeredTopics,
    topicsAfterClose,
    local: {
      sameTrack: published.track === track,
      policy: published.policy,
      contentHint: track.contentHint,
      muted: mutedValue,
      enabledWhenMuted,
    },
    audio: {
      sameTrack: publishedAudio.track === audioTrack,
      policy: publishedAudio.policy,
      contentHint: audioTrack.contentHint,
      maxBitrate: audioSender.encodings[0]?.maxBitrate,
      dtx: audioSender.encodings[0]?.dtx,
    },
    sender,
    replacement: {
      initialTrackId: sender.trackId,
      trackId: replacementSender.trackId,
      expectedTrackId: replacementTrack.id,
      snapshotHasReplacement,
      active: replacementSender.encodings.filter((encoding) => encoding.active).length,
      removedTracks,
    },
    closed: {
      connection: closed.connection,
      localTracks: closed.localPublications.filter((item) => item.track).length,
      remoteVideo: closed.remoteVideo.length,
      notificationsAtClose,
      notificationsAfterClose: closeNotifications.length,
    },
  };
})()
