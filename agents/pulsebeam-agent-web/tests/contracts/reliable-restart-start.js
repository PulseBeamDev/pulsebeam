(async () => {
  const endpoint = "http://127.0.0.1:7070";
  const create = (token) =>
    window.pulsebeam.createAgent({
      endpoint,
      token,
      topology: {
        localVideos: 0,
        localAudios: 0,
        remoteVideos: 1,
        remoteAudios: 0,
      },
    });
  const sender = create("__SENDER_TOKEN__");
  const receiver = create("__RECEIVER_TOKEN__");
  const events = [];
  const remove = receiver.subscribeEvents((event) => {
    if (event.type === "topic-message" && event.mode === "ordered") {
      events.push({
        sequence: Number(event.sequence),
        payload: [...event.payload],
      });
    }
  });
  const wait = (agent, predicate, label) =>
    new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        removeListener();
        reject(
          new Error(
            `timeout waiting for ${label}: ${JSON.stringify(agent.getSnapshot())}`,
          ),
        );
      }, 60000);
      const inspect = () => {
        if (!predicate(agent.getSnapshot())) return;
        clearTimeout(timeout);
        removeListener();
        resolve(agent.getSnapshot());
      };
      const removeListener = agent.subscribe(inspect);
      inspect();
    });
  sender.setState({
    connected: true,
    topics: [{ name: "chat", mode: "ordered", publish: true }],
  });
  receiver.setState({
    connected: true,
    topics: [{ name: "chat", mode: "ordered", subscribe: true }],
  });
  await wait(
    sender,
    (snapshot) => snapshot.topics.publishers[0]?.connected,
    "publisher",
  );
  const before = await wait(
    receiver,
    (snapshot) => snapshot.topics.subscribers[0]?.connected,
    "subscriber",
  );
  await wait(
    receiver,
    (snapshot) =>
      snapshot.catalog.participants.some(
        (participant) => participant.id === sender.getSnapshot().participantId,
      ),
    "publisher catalog",
  );
  sender.sendTopic("chat", "ordered", new Uint8Array([1]));
  await new Promise((resolve, reject) => {
    const deadline = setTimeout(
      () => reject(new Error("initial message missing")),
      20000,
    );
    const check = setInterval(() => {
      if (events.length !== 1) return;
      clearInterval(check);
      clearTimeout(deadline);
      resolve();
    }, 50);
  });
  globalThis.__reliableRestart = {
    sender,
    receiver,
    events,
    remove,
    wait,
    previousGeneration: before.generation,
  };
  return { initial: events[0].payload, generation: Number(before.generation) };
})();
