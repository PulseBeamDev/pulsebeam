import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

export async function publicContract(pulsebeam) {
  const { createAgent, VideoSource } = pulsebeam;
  const { createManagedAgent } = await import(
    "../../node_assets/dist/internal/react.js"
  );
  const closedDuringInitialization = createManagedAgent({
    endpoint: "http://localhost",
    token: "close-before-initialization",
  });
  closedDuringInitialization.activate();
  await closedDuringInitialization.close();
  assert.equal(closedDuringInitialization.isClosed(), true);
  assert.equal(closedDuringInitialization.connection, "disconnected");

  const agent = await createAgent({
    endpoint: "http://localhost",
    token: "public-smoke",
    logging: { level: "off" },
  });
  const borrowed = new Track("video");
  const source = VideoSource.fromTrack(borrowed);
  try {
    assert.equal(
      agent.connection,
      "disconnected",
      "creation is not connection intent",
    );
    const revision = agent.snapshot().desiredRevision;
    const snapshots = agent.snapshots();
    const sibling = agent.snapshots();
    const events = agent.events();
    const siblingEvents = agent.events();
    assert.equal((await snapshots.next()).type, "snapshot");
    assert.equal((await sibling.next()).type, "snapshot");
    assert.equal(
      agent.snapshot().desiredRevision,
      revision,
      "state observation is inert",
    );
    const pendingSnapshot = snapshots.next();
    const queuedSnapshot = snapshots.next();
    const pendingEvent = events.next();
    await snapshots.close();
    await events.close();
    assert.equal((await pendingSnapshot).type, "closed");
    assert.equal((await queuedSnapshot).type, "closed");
    assert.equal((await pendingEvent).type, "closed");
    assert.equal(sibling.isClosed(), false);
    assert.equal(siblingEvents.isClosed(), false);
    await agent.connect();
    await agent.disconnect();
    assert.equal(agent.isClosed(), false, "disconnect is not terminal");
    const camera = agent.video("camera");
    assert.equal(agent.video("camera"), camera);
    for (const removed of [
      "local",
      "topic",
      "setState",
      "reconnect",
      "track",
      "remote",
      "getSnapshot",
      "subscribe",
    ])
      assert.equal(removed in agent, false, `removed public ${removed}`);
    const owner = await camera.publish(source, { enabled: true });
    await owner.close();
    assert.equal(borrowed.readyState, "live", "publication borrows capture");
    const data = await agent.data("chat", "ordered").receive();
    const pending = data.next();
    const pendingSiblingSnapshot = sibling.next();
    const pendingSiblingEvent = siblingEvents.next();
    const closing = agent.close();
    assert.equal(agent.close(), closing);
    await closing;
    assert.equal((await pending).type, "closed");
    assert.equal(data.isClosed(), true);
    assert.equal((await pendingSiblingSnapshot).type, "closed");
    assert.equal((await pendingSiblingEvent).type, "closed");
    assert.equal(sibling.isClosed(), true);
    assert.equal(siblingEvents.isClosed(), true);
    assert.equal((await agent.snapshots().next()).type, "closed");
    assert.equal((await agent.events().next()).type, "closed");
    assert.equal(agent.isClosed(), true);
    const terminal = agent.snapshot();
    await agent.close();
    assert.equal(agent.snapshot(), terminal);
    await assert.rejects(agent.connect(), { code: "closed" });
    return true;
  } finally {
    await agent.close();
    await source.close();
    await closedDuringInitialization.close();
  }
}
