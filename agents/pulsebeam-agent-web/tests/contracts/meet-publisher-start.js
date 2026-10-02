(async () => {
  const { createAgent, createCaptureSource } = await import("__SDK_URL__");
  const { RealContext } = window.__meet;
  const source = new RealContext();
  // The source may start before activation. resume() is retried in the same
  // ordinary gesture as Meet's own recovery, not by a test-only unlock button.
  void source.resume();
  document.addEventListener("click", () => void source.resume(), {
    capture: true,
  });
  const oscillator = source.createOscillator();
  oscillator.frequency.value = 700;
  const destination = source.createMediaStreamDestination();
  oscillator.connect(destination);
  oscillator.start();
  const canvas = document.createElement("canvas");
  canvas.width = 320;
  canvas.height = 180;
  const painter = canvas.getContext("2d");
  let n = 0;
  const timer = setInterval(() => {
    painter.fillStyle = `hsl(${n++ % 360}, 100%, 50%)`;
    painter.fillRect(0, 0, canvas.width, canvas.height);
  }, 30);
  const stream = canvas.captureStream(30);
  const sender = createAgent({
    endpoint: "http://127.0.0.1:7070",
    token: "__SENDER_TOKEN__",
    topology: {
      localVideos: 1,
      localAudios: 1,
      remoteVideos: 0,
      remoteAudios: 0,
    },
  });
  sender.setState({
    connected: false,
    topics: [
      { name: "chat", mode: "ordered", publish: true },
      { name: "reactions", mode: "latest", publish: true },
    ],
  });
  sender.local
    .video("camera")
    .setSource(createCaptureSource(stream.getVideoTracks()[0], "video"));
  sender.local
    .audio("microphone")
    .setSource(
      createCaptureSource(destination.stream.getAudioTracks()[0], "audio"),
    );
  const chat = sender.topic("chat", { mode: "reliable" });
  const reactions = sender.topic("reactions", { mode: "unreliable" });
  const chats = [];
  const emojis = [];
  const abort = new AbortController();
  const receive = async (topic, values) => {
    for await (const value of topic.subscribe({ signal: abort.signal }))
      values.push(value);
  };
  void receive(chat, chats);
  void receive(reactions, emojis);
  sender.connect();
  Object.assign(window.__meet, {
    sender,
    chats,
    emojis,
    source,
    chat,
    reactions,
    stopPublisher: async () => {
      abort.abort();
      sender.close();
      clearInterval(timer);
      stream.getTracks().forEach((track) => track.stop());
      destination.stream.getTracks().forEach((track) => track.stop());
      await source.close();
    },
  });
  return true;
})();
