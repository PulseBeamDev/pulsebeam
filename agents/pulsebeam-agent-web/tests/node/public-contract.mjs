import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

export async function publicContract(pulsebeam) {
  const { createAgent, VideoSource, AgentStatus, ErrorCode, OperationError } =
    pulsebeam;
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
    const initialState = agent.state();
    assert.equal(initialState.status, AgentStatus.Idle);
    assert.equal(agent.state(), initialState);
    assert.equal(Object.isFrozen(initialState), true);
    const stateUpdates = agent.updates();
    const independentState = agent.updates();
    assert.equal(await stateUpdates.next(), initialState);
    assert.equal(await independentState.next(), initialState);
    const pendingState = stateUpdates.next();
    await assert.rejects(stateUpdates.next(), (error) => {
      assert.equal(error instanceof OperationError, true);
      assert.equal(error.info.code, ErrorCode.Conflict);
      assert.equal(Object.isFrozen(error.info), true);
      return true;
    });
    stateUpdates.close();
    assert.equal(await pendingState, undefined);
    const capacity = Array.from({ length: 31 }, () => agent.updates());
    assert.throws(() => agent.updates(), (error) => {
      assert.equal(error instanceof OperationError, true);
      assert.equal(error.info.code, ErrorCode.CapacityExceeded);
      return true;
    });
    for (const cursor of capacity) cursor.close();
    const replacement = agent.updates();
    assert.equal(await replacement.next(), initialState);
    replacement.close();
    assert.equal(agent.state(), initialState, "observation is inert");
    const revision = agent.snapshot().desiredRevision;
    assert.equal(agent.snapshot().joinRevision, 0n);
    assert.equal(agent.snapshot().credentialRevision, 0n);
    assert.equal(agent.snapshot().acceptedCredentialRevision, null);
    assert.equal(agent.snapshot().credentialOutcome, null);
    assert.equal(agent.snapshot().desiredRoom, null);
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
    const joinUpdates = agent.snapshots();
    agent.join("public-contract-room");
    const joinIntent = agent.state();
    assert.equal(joinIntent.desiredJoinRevision, 1n);
    assert.equal(joinIntent.desiredRoom, "public-contract-room");
    assert.equal(initialState.desiredJoinRevision, 0n);
    assert.equal(agent.state(), joinIntent);
    while (true) {
      const update = await joinUpdates.next();
      assert.notEqual(update.type, "closed");
      const current =
        update.type === "snapshot" ? update.snapshot : agent.snapshot();
      if (
        current?.desiredRoom === "public-contract-room" &&
        current.joinRevision === 1n
      )
        break;
    }
    await joinUpdates.close();
    assert.equal(agent.snapshot().desiredRoom, "public-contract-room");
    assert.equal(agent.snapshot().joinRevision, 1n);
    const joinedRevision = agent.snapshot().desiredRevision;
    const disconnectUpdates = agent.snapshots();
    await agent.disconnect();
    // Renewal is queued after disconnect and provides an observable actor barrier.
    await agent.renewAuthorization("public-smoke-renewed");
    while (true) {
      const update = await disconnectUpdates.next();
      assert.notEqual(update.type, "closed");
      const current =
        update.type === "snapshot" ? update.snapshot : agent.snapshot();
      if (current?.credentialRevision === 1n) break;
    }
    await disconnectUpdates.close();
    const disconnectedRevision = agent.snapshot().desiredRevision;
    assert.ok(
      disconnectedRevision > joinedRevision,
      `disconnect after join must advance Core intent: ${joinedRevision} -> ${disconnectedRevision}`,
    );
    const renewedUpdates = agent.snapshots();
    assert.equal(agent.renew("synchronous-renewed"), undefined);
    while (true) {
      const update = await renewedUpdates.next();
      assert.notEqual(update.type, "closed");
      const current = update.type === "snapshot" ? update.snapshot : agent.snapshot();
      if (current?.credentialRevision === 2n) break;
    }
    await renewedUpdates.close();
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
    const finalState = await independentState.next();
    assert.equal(finalState.status, AgentStatus.Closed);
    assert.equal(finalState.room, undefined);
    assert.equal(agent.state(), finalState);
    assert.equal(await independentState.next(), undefined);
    independentState.close();
    const terminalUpdates = agent.updates();
    assert.equal(await terminalUpdates.next(), finalState);
    assert.equal(await terminalUpdates.next(), undefined);
    terminalUpdates.close();
    const terminal = agent.snapshot();
    await agent.close();
    assert.equal(agent.snapshot(), terminal);
    await assert.rejects(agent.connect(), { code: "closed" });
    const unobserved = await createAgent({
      endpoint: "http://localhost",
      token: "close-before-observation",
      logging: { level: "off" },
    });
    await unobserved.close();
    const lateState = unobserved.state();
    assert.equal(lateState.status, AgentStatus.Closed);
    assert.equal(unobserved.state(), lateState);
    const lateUpdates = unobserved.updates();
    assert.equal(await lateUpdates.next(), lateState);
    assert.equal(await lateUpdates.next(), undefined);
    lateUpdates.close();
    return true;
  } finally {
    await agent.close();
    await source.close();
    await closedDuringInitialization.close();
  }
}
