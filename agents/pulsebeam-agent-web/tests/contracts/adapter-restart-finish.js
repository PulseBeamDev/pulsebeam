(async () => {
  const {
    sender,
    receiver,
    publisher,
    stream,
    pumping,
    events,
    wait,
    previousGeneration,
  } = globalThis.__adapterRestart;
  try {
    await wait(
      sender,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration,
      "Web publisher adapter rejoin",
    );
    await wait(
      receiver,
      (snapshot) =>
        snapshot.connection === "connected" &&
        snapshot.generation !== previousGeneration,
      "Web subscriber adapter rejoin",
    );
    await publisher.send(new Uint8Array([2]));
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
    await stream.return();
    await pumping;
    sender.close();
    receiver.close();
    delete globalThis.__adapterRestart;
  }
})();
