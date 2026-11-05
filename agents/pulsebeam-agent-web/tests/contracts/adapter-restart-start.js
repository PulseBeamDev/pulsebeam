(async () => {
  const endpoint = "__DESTINATION_ENDPOINT__";
  const create = (token) =>
    window.pulsebeam.createAgent({
      endpoint,
      token,
    });
  const sender = await create("__SENDER_TOKEN__");
  const receiver = await create("__RECEIVER_TOKEN__");
  const publisher = sender.data("chat", "lossy");
  const stream = await receiver
    .participant("publisher")
    .data("chat", "lossy")
    .receive();
  const events = [];
  let probe = 0;
  let observedProbe = 0;
  const pumping = (async () => {
    while (true) {
      const update = await stream.next();
      if (update.type === "closed") return;
      const event = update.event;
      if (event.type === "message") {
        if (event.data.length === 2 && event.data[0] === 0) {
          observedProbe = Math.max(observedProbe, event.data[1]);
        } else {
          events.push({ sequence: event.sequence, payload: [...event.data] });
        }
      }
    }
  })();
  const wait = async (agent, predicate, label) => {
    const snapshots = agent.snapshots();
    let timeout;
    try {
      return await new Promise((resolve, reject) => {
        timeout = setTimeout(() => {
          void snapshots.close();
          reject(
            new Error(
              `timeout waiting for ${label}: ${JSON.stringify(agent.snapshot(), (_, value) => (typeof value === "bigint" ? value.toString() : value))}`,
            ),
          );
        }, 60000);
        void (async () => {
          while (true) {
            const update = await snapshots.next();
            if (update.type === "closed")
              throw new Error(`closed while waiting for ${label}`);
            const snapshot =
              update.type === "snapshot" ? update.snapshot : agent.snapshot();
            if (predicate(snapshot)) {
              resolve(snapshot);
              return;
            }
          }
        })().catch(reject);
      });
    } finally {
      clearTimeout(timeout);
      await snapshots.close();
    }
  };
  const synchronize = async () => {
    const nonce = ++probe;
    const deadline = Date.now() + 20000;
    // Lossy admission is not subscription readiness; observe real application traffic.
    while (observedProbe < nonce) {
      await publisher.send(new Uint8Array([0, nonce]));
      if (observedProbe >= nonce) break;
      if (Date.now() > deadline)
        throw new Error(
          `lossy application synchronization missing: ${JSON.stringify({ sender: sender.snapshot(), receiver: receiver.snapshot() }, (_, value) => (typeof value === "bigint" ? value.toString() : value))}`,
        );
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  };
  await sender.connect();
  await receiver.connect();
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
