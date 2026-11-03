(async () => {
  const endpoint = "__DESTINATION_ENDPOINT__";
  const create = (token) =>
    window.pulsebeam.createAgent({
      endpoint,
      token,
    });
  const sender = create("__SENDER_TOKEN__");
  const receiver = create("__RECEIVER_TOKEN__");
  const spec = window.pulsebeam.track.data("chat", "lossy");
  const publisher = sender.track(spec);
  const stream = receiver.remote.participant("publisher").track(spec).events();
  const events = [];
  let probe = 0;
  let observedProbe = 0;
  const pumping = (async () => {
    for await (const event of stream) {
      if (event.type === "message") {
        if (event.data.length === 2 && event.data[0] === 0) {
          observedProbe = Math.max(observedProbe, event.data[1]);
        } else {
          events.push({ sequence: event.sequence, payload: [...event.data] });
        }
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
  const synchronize = async () => {
    const nonce = ++probe;
    const deadline = Date.now() + 20000;
    // Lossy admission is not subscription readiness; observe real application traffic.
    while (observedProbe < nonce) {
      await publisher.send(new Uint8Array([0, nonce]));
      if (observedProbe >= nonce) break;
      if (Date.now() > deadline)
        throw new Error(
          `lossy application synchronization missing: ${JSON.stringify({ sender: sender.getSnapshot(), receiver: receiver.getSnapshot() }, (_, value) => (typeof value === "bigint" ? value.toString() : value))}`,
        );
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  };
  sender.connect();
  receiver.connect();
  const before = await wait(
    receiver,
    (snapshot) => snapshot.connection === "connected",
    "subscriber",
  );
  await synchronize();
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
    synchronize,
    previousGeneration: before.generation,
  };
  return { initial: events[0].payload, generation: Number(before.generation) };
})();
