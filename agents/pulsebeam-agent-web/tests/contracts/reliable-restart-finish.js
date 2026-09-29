(async () => {
  const { sender, receiver, events, remove, wait, previousGeneration } =
    globalThis.__reliableRestart;
  await wait(
    sender,
    (snapshot) =>
      snapshot.connection === "connected" &&
      snapshot.generation !== previousGeneration,
    "publisher rejoin",
  );
  await wait(
    receiver,
    (snapshot) =>
      snapshot.connection === "connected" &&
      snapshot.generation !== previousGeneration,
    "receiver rejoin",
  );
  await new Promise((resolve, reject) => {
    const deadline = setTimeout(
      () => reject(new Error(`recovery missing: ${JSON.stringify(events)}`)),
      60000,
    );
    const poll = setInterval(() => {
      if (events.length < 2) return;
      clearTimeout(deadline);
      clearInterval(poll);
      resolve();
    }, 50);
  });
  sender.sendTopic("chat", "ordered", new Uint8Array([3]));
  await new Promise((resolve, reject) => {
    const deadline = setTimeout(
      () => reject(new Error(`later send missing: ${JSON.stringify(events)}`)),
      20000,
    );
    const poll = setInterval(() => {
      if (events.length < 3) return;
      clearTimeout(deadline);
      clearInterval(poll);
      resolve();
    }, 50);
  });
  const late = window.pulsebeam.createAgent({
    endpoint: "http://127.0.0.1:7070",
    token: "__LATE_TOKEN__",
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 1,
      remoteAudios: 0,
    },
  });
  const lateEvents = [];
  const removeLate = late.subscribeEvents((event) => {
    if (event.type === "topic-message" && event.mode === "ordered")
      lateEvents.push([...event.payload]);
  });
  late.setState({
    connected: true,
    topics: [{ name: "chat", mode: "ordered", subscribe: true }],
  });
  await wait(
    late,
    (snapshot) => snapshot.topics.subscribers[0]?.connected,
    "late subscriber",
  );
  await wait(
    late,
    (snapshot) =>
      snapshot.catalog.participants.some(
        (participant) => participant.id === sender.getSnapshot().participantId,
      ),
    "late subscriber catalog",
  );
  await new Promise((resolve) => setTimeout(resolve, 2500));
  const noHistoricalReplay = lateEvents.length === 0;
  sender.sendTopic("chat", "ordered", new Uint8Array([4]));
  await new Promise((resolve, reject) => {
    const deadline = setTimeout(
      () =>
        reject(
          new Error(
            `late subscriber missed live send: ${JSON.stringify(lateEvents)}`,
          ),
        ),
      20000,
    );
    const poll = setInterval(() => {
      if (lateEvents.length < 1 || events.length < 4) return;
      clearTimeout(deadline);
      clearInterval(poll);
      resolve();
    }, 50);
  });
  const result = {
    events,
    lateEvents,
    noHistoricalReplay,
    rejoined:
      sender.getSnapshot().generation !== previousGeneration &&
      receiver.getSnapshot().generation !== previousGeneration,
  };
  remove();
  removeLate();
  await sender.close();
  await receiver.close();
  await late.close();
  delete globalThis.__reliableRestart;
  return result;
})();
