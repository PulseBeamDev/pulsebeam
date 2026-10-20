(async () => {
  const endpoint = "__DESTINATION_ENDPOINT__";
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
    if (event.type === "topic-message" && event.mode === "lossy") {
      events.push({
        sequence: event.sequence,
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
            `timeout waiting for ${label}: ${JSON.stringify(agent.getSnapshot(), (_, value) => (typeof value === "bigint" ? value.toString() : value))}`,
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
    topics: [{ name: "chat", mode: "lossy", publish: true }],
  });
  receiver.setState({
    connected: true,
    topics: [{ name: "chat", mode: "lossy", subscribe: true }],
  });
  await wait(
    sender,
    (snapshot) =>
      snapshot.topics.publishers[0]?.connected &&
      snapshot.topics.publishers[0].dataTrackId !== null,
    "publisher",
  );
  const before = await wait(
    receiver,
    (snapshot) =>
      snapshot.topics.subscribers[0]?.connected &&
      snapshot.topics.subscribers[0].publishers === 1,
    "subscriber",
  );
  await sender.sendTopic("chat", "lossy", new Uint8Array([1]));
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
  globalThis.__adapterRestart = {
    sender,
    receiver,
    events,
    remove,
    wait,
    previousGeneration: before.generation,
  };
  return { initial: events[0].payload, generation: Number(before.generation) };
})();
