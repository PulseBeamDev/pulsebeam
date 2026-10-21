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
  const spec = window.pulsebeam.track.data("chat", "lossy");
  const publisher = sender.track(spec);
  const stream = receiver.remote.participant("publisher").track(spec).events();
  const events = [];
  const pumping = (async () => {
    for await (const event of stream) {
      if (event.type === "message") {
        events.push({ sequence: event.sequence, payload: [...event.data] });
      }
    }
  })();
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
  sender.connect();
  receiver.connect();
  const before = await wait(
    receiver,
    (snapshot) => snapshot.connection === "connected",
    "subscriber",
  );
  await publisher.send(new Uint8Array([1]));
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
    publisher,
    stream,
    pumping,
    events,
    wait,
    previousGeneration: before.generation,
  };
  return { initial: events[0].payload, generation: Number(before.generation) };
})();
