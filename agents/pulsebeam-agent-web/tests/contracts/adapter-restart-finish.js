(async () => {
  const { sender, receiver, events, remove, wait, previousGeneration } =
    globalThis.__adapterRestart;
  try {
    await wait(
      sender,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration &&
        snapshot.topics.publishers[0]?.connected &&
        snapshot.topics.publishers[0].dataTrackId !== null,
      "Web publisher adapter rejoin",
    );
    await wait(
      receiver,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration &&
        snapshot.topics.subscribers[0]?.connected &&
        snapshot.topics.subscribers[0].publishers === 1,
      "Web subscriber adapter rejoin",
    );
    await sender.sendTopic("chat", "lossy", new Uint8Array([2]));
    const deadline = Date.now() + 20000;
    while (!events.some((event) => event.payload.join(",") === "2")) {
      if (Date.now() > deadline)
        throw new Error("fresh Web adapter delivery missing after restart");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    return (
      receiver.connection === "connected" && sender.connection === "connected"
    );
  } finally {
    remove();
    sender.close();
    receiver.close();
    delete globalThis.__adapterRestart;
  }
})();
