import assert from "node:assert/strict";
import { Agent, AudioInput, InputFrame, NativeError, ReceiveMode, VideoInput } from "@pulsebeam/agent/native";
import { LogLevel, PlayoutDelay, TopicMode, TrackSpec } from "@pulsebeam/agent/core";

const agent = await Agent.create({
  session: {
    endpoint: process.argv[2],
    token: process.argv[3],
    retry: { initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 2 },
    logLevel: LogLevel.Warn,
  },
  media: { audioInput: new AudioInput.Pcm(), videoInput: VideoInput.I420, videoPublications: [] },
});
try {
  const updates = agent.snapshots();
  const events = agent.events();
  const topic = { name: "foreign-exchange", mode: TopicMode.Ordered };
  const publication = await agent.publication(new TrackSpec.Video({ label: "node-camera" }));
  const camera = await publication.input();
  await agent.replaceDesired({
    connected: true,
    publications: [],
    video: [],
    audio: { pinned: [], selected: [], automatic: false, playoutDelays: [], selectorDelays: [] },
    topicPublishers: [topic],
    topicSubscribers: [topic],
  });
  while (true) {
    const update = await updates.next();
    assert.equal(update.tag, "Snapshot");
    const snapshot = update.inner.snapshot;
    assert.notEqual(snapshot.connection.tag, "TerminalFailure");
    if (snapshot.connection.tag === "Connected" && snapshot.participantId &&
        snapshot.topics.publishers.some((p) => p.connected && p.dataTrackId) &&
        snapshot.topics.subscribers.some((s) => s.connected && s.publishers > 0n)) {
      assert.equal(typeof snapshot.generation, "bigint");
      break;
    }
  }
  await camera.ready();
  const timestampUs = await camera.captureTimeUs();
  assert.equal(typeof timestampUs, "bigint");
  assert(timestampUs > 0n);
  const data = new Uint8Array(160 * 96 + 2 * 80 * 48);
  data.fill(180, 0, 160 * 96);
  data.fill(90, 160 * 96, 160 * 96 + 80 * 48);
  data.fill(180, 160 * 96 + 80 * 48);
  await camera.submit(new InputFrame.I420({ frame: { width: 160, height: 96, timestampUs, data: data.buffer, rotationDegrees: 0 } }));

  const sent = Uint8Array.from([...Buffer.from("node-bytes-"), 0, 255]);
  let replied = false;
  const sending = (async () => {
    while (!replied) {
      await agent.sendTopic(topic, sent.buffer);
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  })();
  try {
    while (true) {
      const update = await Promise.race([events.next(), sending]);
      assert.notEqual(update.tag, "Lagged");
      assert.notEqual(update.tag, "Closed");
      if (update.tag !== "Event" || update.inner.event.tag !== "Core") continue;
      const notification = update.inner.event.inner.notification;
      if (notification.tag !== "Topic" || notification.inner.notification.tag !== "Message") continue;
      const message = notification.inner.notification.inner.message;
      if (message.name === "foreign-exchange" && Buffer.from(message.payload).equals(Buffer.concat([Buffer.from("rust-ack:"), Buffer.from(sent)]))) {
        assert.equal(typeof message.sequence, "bigint");
        break;
      }
    }
  } finally {
    replied = true;
    await sending;
  }
  const interest = await agent.videoSubscription(
    { participantId: "publisher", label: "production-camera" },
    { visible: true, height: 96, minHeight: 0, minFps: 0, priority: 1, playoutDelay: new PlayoutDelay.Adaptive() },
  );
  const receiver = await interest.observe(ReceiveMode.Decoded);
  let pixels;
  while (true) {
    const update = await receiver.next();
    assert.notEqual(update.tag, "Closed");
    assert.notEqual(update.tag, "Lagged");
    if (update.tag === "Frame" && update.inner.frame.frame.tag === "I420") {
      pixels = update.inner.frame.frame.inner.frame;
      assert.equal(pixels.width, 160);
      assert.equal(pixels.height, 96);
      assert.equal(pixels.data.byteLength, 160 * 96 + 2 * 80 * 48);
      assert.equal(typeof pixels.timestampUs, "bigint");
      break;
    }
  }
  const retained = Uint8Array.from(new Uint8Array(pixels.data));
  await receiver.close();
  await interest.close();
  assert.deepEqual(new Uint8Array(pixels.data), retained);
  await agent.close();
  assert.equal(agent.snapshot().connection.tag, "Disconnected");
  assert.equal(agent.statistics(), undefined);
  await assert.rejects(camera.captureTimeUs(), (error) => NativeError.NotReady.instanceOf(error));
  await publication.close();
  assert(camera.isClosed());
  console.log("installed Node topic/media/real SFU/owned-close PASS");
} finally {
  await agent.abort();
}
