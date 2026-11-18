import assert from "node:assert/strict";
import { Agent, InputFrame, NativeError, PublicationUpdates, ReceiveMode } from "@pulsebeam/agent/native";
import { ErrorCode, LogLevel, PlayoutDelay, PublicationAdmissionStatus, PublicationDemand, TopicMode } from "@pulsebeam/agent/core";

const room = process.argv[4];
assert.ok(room, "an explicit room is required");
const agent = await Agent.create({
  endpoint: process.argv[2],
  token: process.argv[3],
  retry: { initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 2 },
  logLevel: LogLevel.Warn,
});
try {
  const updates = agent.snapshots();
  const exchange = agent.data("foreign-exchange", TopicMode.Ordered);
  const replies = await agent.participant("publisher").data("foreign-exchange", TopicMode.Ordered).receive();
  const camera = await agent.createVideoInput({
    encodings: [{ rid: undefined, scaleDownBy: 1, maxBitrateBps: undefined, maxFramerate: undefined, temporalLayers: 1 }],
    dependencyDescriptor: true,
  });
  const publication = await agent.video("node-camera").publish(camera, { enabled: true });
  const publicationUpdates = publication.updates();
  assert(PublicationUpdates.instanceOf(publicationUpdates));
  const pendingPublication = publication.state();
  assert.equal(pendingPublication.admission.status, PublicationAdmissionStatus.Pending);
  assert.equal(pendingPublication.admission.demand, PublicationDemand.Unknown);
  assert.equal((await publicationUpdates.next()).observationRevision, pendingPublication.observationRevision);
  agent.join(room);
  while (true) {
    const update = await updates.next();
    assert.equal(update.tag, "Snapshot");
    const snapshot = update.inner.snapshot;
    assert.notEqual(snapshot.connection.tag, "TerminalFailure");
    if (snapshot.connection.tag === "Connected" && snapshot.participantId) {
      assert.equal(typeof snapshot.generation, "bigint");
      break;
    }
  }
  console.log("installed Node RTC connected");
  await publication.ready();
  let admittedPublication = publication.state();
  while (admittedPublication.admission.status !== PublicationAdmissionStatus.Admitted) {
    admittedPublication = await publicationUpdates.next();
    assert(admittedPublication, "publication observation ended before server confirmation");
  }
  assert.equal(admittedPublication.admission.demand, PublicationDemand.Idle);
  assert.equal(typeof admittedPublication.admission.publicationId, "string");
  assert.equal(admittedPublication.admission.connectionGeneration, agent.snapshot().generation);
  const timestampUs = await camera.captureTimeUs();
  assert.equal(typeof timestampUs, "bigint");
  assert(timestampUs > 0n);
  const data = new Uint8Array(160 * 96 + 2 * 80 * 48);
  data.fill(180, 0, 160 * 96);
  data.fill(90, 160 * 96, 160 * 96 + 80 * 48);
  data.fill(180, 160 * 96 + 80 * 48);
  await camera.submit(new InputFrame.I420({ frame: { width: 160, height: 96, timestampUs, data, rotationDegrees: 0 } }));

  const sent = Uint8Array.from([...Buffer.from("node-bytes-"), 0, 255]);
  let replied = false;
  const sending = (async () => {
    while (!replied) {
      await exchange.send(sent);
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  })();
  try {
    while (true) {
      const update = await Promise.race([replies.next(), sending]);
      assert.equal(update.tag, "Event");
      const event = update.inner.event;
      assert.equal(event.tag, "Message");
      const message = event.inner;
      assert.equal(message.participantId, "publisher");
      if (Buffer.from(message.data).equals(Buffer.concat([Buffer.from("rust-ack:"), Buffer.from(sent)]))) {
        assert.equal(typeof message.sequence, "bigint");
        break;
      }
    }
  } finally {
    replied = true;
    await sending;
  }
  console.log("installed Node owned binary data exchanged");
  await replies.close();
  const interest = await agent.participant("publisher").video("production-camera").subscribe(
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
  console.log("installed Node Rust I420 decoded");
  const retained = Uint8Array.from(new Uint8Array(pixels.data));
  await receiver.close();
  await interest.close();
  assert.deepEqual(new Uint8Array(pixels.data), retained);
  await agent.disconnect();
  assert.equal(agent.snapshot().connection.tag, "Disconnected");
  assert.equal(agent.isClosed(), false);
  assert.equal(agent.statistics(), undefined);
  assert.equal(camera.isClosed(), false);
  await assert.rejects(camera.captureTimeUs(), (error) => NativeError.NotReady.instanceOf(error));
  await publication.close();
  assert.equal(publication.state().closed, true);
  assert.equal((await publicationUpdates.next()).closed, true);
  assert.equal(await publicationUpdates.next(), undefined);
  publicationUpdates.close();
  assert.equal(camera.isClosed(), false);
  await camera.close();
  assert(camera.isClosed());
  await agent.close();
  assert(agent.isClosed());
  assert.throws(() => agent.join(room), (error) =>
    NativeError.Operation.instanceOf(error) && error.inner.info.code === ErrorCode.Closed,
  );
  console.log("installed Node owned data/media/real SFU/independent source/terminal close PASS");
} finally {
  await agent.close();
}
