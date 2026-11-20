import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";
import { Track } from "./platform.mjs";

export async function publicContract(pulsebeam) {
  const { Agent, VideoSource, AgentStatus, ErrorCode, OperationError } =
    pulsebeam;
  const { createManagedAgent } = await import(
    "../../node_assets/dist/internal/react.js"
  );
  assert.equal("createAgent" in pulsebeam, false);
  const immediate = new Agent({
    endpoint: "http://localhost",
    token: "join-immediately-after-construction",
    logging: { level: "off" },
  });
  immediate.join("constructor-room");
  assert.equal(immediate.state().desiredJoinRevision, 1n);
  await setImmediate();
  assert.notEqual(
    immediate.state().status,
    AgentStatus.Idle,
    "constructor defaults must not overtake an immediately submitted join",
  );
  await immediate.close();
  for (const invalid of [
    { endpoint: "private-configuration-value", token: "public-smoke" },
    { endpoint: "http://localhost", token: "" },
    {
      endpoint: "http://localhost",
      token: "public-smoke",
      logging: { level: "private-configuration-value" },
    },
  ]) {
    assert.throws(
      () => new Agent(invalid),
      (error) => {
        assert.equal(error instanceof OperationError, true);
        assert.equal(error.info.code, ErrorCode.InvalidArgument);
        assert.equal(
          error.message.includes("private-configuration-value"),
          false,
        );
        assert.equal(Object.isFrozen(error.info), true);
        return true;
      },
    );
  }
  const closedDuringActivation = createManagedAgent({
    endpoint: "http://localhost",
    token: "close-during-activation",
  });
  closedDuringActivation.subscribe(() => {
    if (!closedDuringActivation.isClosed()) void closedDuringActivation.close();
  });
  closedDuringActivation.activate();
  assert.equal(closedDuringActivation.isClosed(), true);
  await closedDuringActivation.close();
  assert.equal(closedDuringActivation.connection, "disconnected");

  const reactivated = createManagedAgent({
    endpoint: "http://localhost",
    token: "reactivate-during-activation",
  });
  let replaced = false;
  reactivated.subscribe(() => {
    if (replaced) return;
    replaced = true;
    reactivated.deactivate();
    reactivated.activate();
  });
  reactivated.activate();
  assert.equal(replaced, true);
  await reactivated.initialized();
  assert.equal(reactivated.state().status, AgentStatus.Idle);
  await reactivated.close();

  const recoveredActivation = createManagedAgent({
    endpoint: "http://localhost",
    token: "recover-during-activation-failure",
  });
  recoveredActivation.setState({
    connected: false,
    topics: [{ name: "chat", mode: "invalid", publish: true }],
  });
  let recovered = false;
  recoveredActivation.subscribe(() => {
    if (recovered || recoveredActivation.connection !== "terminal-failure")
      return;
    recovered = true;
    recoveredActivation.deactivate();
    recoveredActivation.setState({ connected: false });
    recoveredActivation.activate();
  });
  recoveredActivation.activate();
  assert.equal(recovered, true);
  assert.doesNotThrow(
    () => recoveredActivation.renew("reopened-credential"),
    "retired activation cleanup must not detach the replacement Core",
  );
  await recoveredActivation.initialized();
  assert.equal(recoveredActivation.state().status, AgentStatus.Idle);
  assert.equal(recoveredActivation.error, null);
  await recoveredActivation.close();

  const agent = new Agent({
    endpoint: "http://localhost",
    token: "public-smoke",
    logging: { level: "off" },
  });
  assert.equal(agent instanceof Agent, true);
  assert.equal(agent instanceof Promise, false);
  const borrowed = new Track("video");
  const source = VideoSource.fromTrack(borrowed);
  try {
    assert.equal(
      agent.connection,
      "disconnected",
      "creation is not connection intent",
    );
    const initialState = agent.state();
    const support = agent.supportReport();
    assert.deepEqual(support, { connectionId: undefined });
    assert.ok(Object.isFrozen(support));
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
    assert.throws(
      () => agent.updates(),
      (error) => {
        assert.equal(error instanceof OperationError, true);
        assert.equal(error.info.code, ErrorCode.CapacityExceeded);
        return true;
      },
    );
    for (const cursor of capacity) cursor.close();
    const replacement = agent.updates();
    assert.equal(await replacement.next(), initialState);
    replacement.close();
    assert.equal(agent.state(), initialState, "observation is inert");
    const participant = agent.room("observation-room").participant("alice");
    const equivalent = agent.room("observation-room").participant("alice");
    assert.equal(participant, equivalent);
    assert.equal(participant.video("camera"), equivalent.video("camera"));
    assert.equal(participant.audio("voice"), equivalent.audio("voice"));
    assert.notEqual(
      participant.video("camera"),
      agent.room("other-room").participant("alice").video("camera"),
    );
    assert.notEqual(
      participant.video("camera"),
      agent.room("observation-room").participant("bob").video("camera"),
    );
    const defaultVideo = participant.video("default-camera").subscribe();
    const defaultAudio = participant.audio("default-microphone").subscribe();
    assert.equal(defaultVideo.state().localEnabled, true);
    assert.equal(defaultVideo.state().configuration.visible, true);
    assert.deepEqual(defaultAudio.state().playoutDelay, { tag: "Adaptive" });
    await defaultVideo.configure({
      visible: false,
      height: 10,
      minHeight: 0,
      minFps: 0,
      priority: 1,
      playoutDelay: { mode: "fixed", minMs: 20, maxMs: 20 },
    });
    await defaultAudio.configure({
      playoutDelay: { mode: "fixed", minMs: 20, maxMs: 20 },
    });
    await defaultVideo.configure();
    await defaultAudio.configure();
    assert.equal(defaultVideo.state().configuration.visible, true);
    assert.deepEqual(defaultVideo.state().configuration.playoutDelay, {
      tag: "Adaptive",
    });
    assert.deepEqual(defaultAudio.state().playoutDelay, { tag: "Adaptive" });
    await defaultVideo.close();
    await defaultAudio.close();
    const videoConfig = {
      visible: true,
      height: 720,
      minHeight: 360,
      minFps: 15,
      priority: 100,
      playoutDelay: { mode: "fixed", minMs: 10, maxMs: 40 },
    };
    const audioConfig = {
      playoutDelay: { mode: "fixed", minMs: 20, maxMs: 50 },
    };
    const subscriptions = [
      {
        handle: participant.video("camera").subscribe(videoConfig),
        selector: {
          room: "observation-room",
          participantId: "alice",
          trackId: "camera",
        },
        initial: {
          ...videoConfig,
          playoutDelay: { tag: "Fixed", inner: { minMs: 10, maxMs: 40 } },
        },
        changed: { ...videoConfig, height: 480 },
        expected: {
          ...videoConfig,
          height: 480,
          playoutDelay: { tag: "Fixed", inner: { minMs: 10, maxMs: 40 } },
        },
        field: "configuration",
      },
      {
        handle: participant.audio("microphone").subscribe(audioConfig),
        selector: {
          room: "observation-room",
          participantId: "alice",
          trackId: "microphone",
        },
        initial: { tag: "Fixed", inner: { minMs: 20, maxMs: 50 } },
        changed: { playoutDelay: { mode: "adaptive" } },
        expected: { tag: "Adaptive" },
        field: "playoutDelay",
      },
    ];
    for (const {
      handle,
      selector,
      initial,
      changed,
      expected,
      field,
    } of subscriptions) {
      const first = handle.state();
      assert.equal(first.closed, false);
      assert.deepEqual(first.selector, selector);
      assert.deepEqual(first[field], initial);
      assert.equal(
        first.sourceStatus,
        pulsebeam.SubscriptionSourceStatus.Pending,
      );
      assert.equal(first.sourceTrackId, undefined);
      assert.equal(handle.state(), first);
      assert.equal(Object.isFrozen(first), true);
      assert.equal(Object.isFrozen(first.selector), true);
      assert.equal(Object.isFrozen(first[field]), true);
      const delay =
        field === "configuration"
          ? first.configuration.playoutDelay
          : first.playoutDelay;
      assert.equal(Object.isFrozen(delay), true);
      assert.equal(Object.isFrozen(delay.inner), true);
      assert.throws(() => {
        first.selector.trackId = "other";
      }, TypeError);
      const cursor = handle.updates();
      const siblingCursor = handle.updates();
      assert.equal(await cursor.next(), first);
      assert.equal(await siblingCursor.next(), first);
      cursor.close();
      assert.equal(handle.isClosed(), false);
      await handle.configure(changed);
      const configured = handle.state();
      assert.deepEqual(configured[field], expected);
      assert.ok(configured.observationRevision > first.observationRevision);
      assert.equal(handle.state(), configured);
      assert.equal(await siblingCursor.next(), configured);
      for (let index = 0; index < 16; ++index) {
        const abort = new AbortController();
        const cancelled = assert.rejects(
          handle.observe(abort.signal),
          (error) => error.code === "closed",
        );
        await setImmediate();
        abort.abort();
        await cancelled;
        assert.equal(
          handle.isClosed(),
          false,
          "cancelling output acquisition preserves reception",
        );
      }
      const alreadyAborted = new AbortController();
      alreadyAborted.abort();
      await assert.rejects(
        handle.observe(alreadyAborted.signal),
        (error) => error.code === "closed",
      );
      await handle.close();
      const final = handle.state();
      assert.equal(final.closed, true);
      assert.equal(handle.state(), final);
      assert.equal(await siblingCursor.next(), final);
      assert.equal(await siblingCursor.next(), undefined);
      siblingCursor.close();
    }
    const localData = agent.data("owned-events", "ordered");
    const publication = localData.publish();
    const dataSubscription = participant
      .data("owned-events", "ordered")
      .subscribe();
    const unrelatedData = participant.data("other-events", "lossy").subscribe();
    assert.equal(publication instanceof Promise, false);
    assert.equal(dataSubscription instanceof Promise, false);
    const publicationState = publication.state();
    const subscriptionState = dataSubscription.state();
    assert.equal(publicationState.trackId, "owned-events");
    assert.equal(publicationState.delivery, 2); // Core TopicMode.Ordered
    assert.deepEqual(subscriptionState.selector, {
      room: "observation-room",
      participantId: "alice",
      trackId: "owned-events",
    });
    assert.equal(subscriptionState.delivery, publicationState.delivery);
    assert.equal(unrelatedData.state().delivery, 1); // Core TopicMode.Lossy
    assert.throws(
      () => localData.publish(),
      (error) => {
        assert.ok(error instanceof OperationError);
        assert.equal(error.info.code, ErrorCode.Conflict);
        return true;
      },
    );
    await assert.rejects(publication.send(Uint8Array.of(0, 255)), (error) => {
      assert.ok(error instanceof OperationError);
      assert.equal(error.info.code, ErrorCode.NotReady);
      assert.equal(Object.isFrozen(error.info), true);
      return true;
    });
    const publicationUpdates = publication.updates();
    const subscriptionUpdates = dataSubscription.updates();
    assert.equal(await publicationUpdates.next(), publicationState);
    assert.equal(await subscriptionUpdates.next(), subscriptionState);
    const pendingData = dataSubscription.next();
    await dataSubscription.close();
    assert.deepEqual(await pendingData, { type: "closed" });
    const closedSubscription = dataSubscription.state();
    assert.equal(closedSubscription.closed, true);
    assert.equal(await subscriptionUpdates.next(), closedSubscription);
    assert.equal(await subscriptionUpdates.next(), undefined);
    assert.equal(unrelatedData.state().closed, false);
    assert.equal(publication.state().closed, false);
    await publication.close();
    const closedPublication = publication.state();
    assert.equal(closedPublication.closed, true);
    assert.equal(await publicationUpdates.next(), closedPublication);
    assert.equal(await publicationUpdates.next(), undefined);
    assert.equal(unrelatedData.state().closed, false);
    publicationUpdates.close();
    subscriptionUpdates.close();
    await unrelatedData.close();
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
      const current =
        update.type === "snapshot" ? update.snapshot : agent.snapshot();
      if (current?.credentialRevision === 2n) break;
    }
    await renewedUpdates.close();
    await agent.connect();
    await agent.disconnect();
    assert.equal(agent.isClosed(), false, "disconnect is not terminal");
    const beforeRoomLookup = agent.snapshot();
    const room = agent.room("conference-42");
    const pinnedParticipant = room.participant("alice");
    assert.equal(room.name, "conference-42");
    assert.equal(pinnedParticipant.room, "conference-42");
    assert.equal(pinnedParticipant.participantId, "alice");
    assert.deepEqual(
      agent.snapshot(),
      beforeRoomLookup,
      "room lookup is inert",
    );
    const camera = agent.video("camera");
    assert.equal(agent.video("camera"), camera);
    for (const removed of [
      "local",
      "topic",
      "setState",
      "reconnect",
      "track",
      "remote",
      "participant",
      "getSnapshot",
      "subscribe",
    ])
      assert.equal(removed in agent, false, `removed public ${removed}`);
    const owner = camera.publish({ enabled: true });
    assert.equal(owner instanceof Promise, false);
    const input = source.pipeTo(owner);
    assert.equal(input instanceof Promise, false);
    await input.close();
    await owner.close();
    assert.equal(borrowed.readyState, "live", "publication borrows capture");
    assert.equal("receive" in agent.data("chat", "ordered"), false);
    assert.equal("send" in agent.data("chat", "ordered"), false);
    assert.equal(
      "receive" in
        agent.room("room").participant("alice").data("chat", "ordered"),
      false,
    );
    const data = await agent
      .room("room")
      .participant("alice")
      .data("chat", "ordered")
      .subscribe();
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
    const unobserved = new Agent({
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
    await closedDuringActivation.close();
  }
}
