(async () => {
  const { sender, receiver, events, remove, wait, previousGeneration } =
    globalThis.__reliableRestart;
  try {
    await wait(
      sender,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration &&
        snapshot.topics.publishers[0]?.connected,
      "Web publisher adapter rejoin",
    );
    await wait(
      receiver,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration &&
        snapshot.topics.subscribers[0]?.connected,
      "Web subscriber adapter rejoin",
    );
    sender.sendTopic("chat", "ordered", new Uint8Array([2]));
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
    delete globalThis.__reliableRestart;
  }
})();
