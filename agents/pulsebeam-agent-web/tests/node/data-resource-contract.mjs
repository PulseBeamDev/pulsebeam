import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";

export async function dataResourceContract(pulsebeam) {
  const config = {
    endpoint: "http://localhost",
    token: "owned-binary",
    logging: { level: "off" },
  };
  const agent = new pulsebeam.Agent(config);
  try {
    const local = agent.data("events", "ordered");
    assert.equal(Object.isFrozen(local), true);
    assert.equal(local, agent.data("events", "ordered"));
    assert.notEqual(local, agent.data("events", "latest"));
    const other = new pulsebeam.Agent(config);
    try {
      const otherLocal = other.data("events", "ordered");
      assert.notEqual(local, otherLocal);
      const pendingAdmission = other
        .room("room")
        .participant("pending")
        .data("events", "ordered")
        .subscribe();
      await other.close();
      const closedDuringAdmission = await pendingAdmission;
      assert.equal(closedDuringAdmission.isClosed(), true);
      assert.deepEqual(await closedDuringAdmission.next(), { type: "closed" });
      await closedDuringAdmission.close();
    } finally {
      await other.close();
    }
    const alice = agent
      .room("room")
      .participant("alice")
      .data("events", "ordered");
    const bob = agent.room("room").participant("bob").data("events", "ordered");
    assert.equal(
      alice,
      agent.room("room").participant("alice").data("events", "ordered"),
    );
    assert.notEqual(alice, bob);
    assert.equal(alice.id, null);
    assert.equal(alice.active, false);
    const publication = local.publish();
    await assert.rejects(publication.send(Uint8Array.of(255)), (error) => {
      assert.ok(error instanceof pulsebeam.OperationError);
      assert.equal(error.info.code, pulsebeam.ErrorCode.NotReady);
      assert.equal(Object.isFrozen(error.info), true);
      return true;
    });

    const longLabel = "x".repeat(36);
    const longNamed = agent.data(longLabel, "ordered");
    assert.equal(
      longNamed.trackId,
      longLabel,
      "local track IDs preserve their exact maximum-length name",
    );
    for (const trackId of ["", "x".repeat(37), "é", "chat/updates"]) {
      assert.throws(
        () =>
          agent
            .room("room")
            .participant("alice")
            .data(trackId, "ordered")
            .subscribe(),
        (error) => {
          assert.ok(error instanceof pulsebeam.OperationError);
          assert.equal(error.info.code, pulsebeam.ErrorCode.InvalidArgument);
          return true;
        },
      );
    }
    const all = await agent
      .room("room")
      .participant("carol")
      .data("events", "ordered")
      .subscribe();
    const scoped = alice.subscribe();
    const unrelated = bob.subscribe();
    assert.equal(all.state().selector.participantId, "carol");
    assert.equal(scoped.state().selector.participantId, "alice");
    assert.equal(unrelated.state().selector.participantId, "bob");
    let allResolved = false;
    const allRead = all.next().then((value) => {
      allResolved = true;
      return value;
    });
    const scopedReads = [scoped.next(), scoped.next()];
    const unrelatedRead = unrelated.next();
    await scoped.close();
    assert.equal(scoped.isClosed(), true);
    assert.deepEqual(await Promise.all(scopedReads), [
      { type: "closed" },
      { type: "closed" },
    ]);
    assert.equal(allResolved, false);
    assert.equal(all.isClosed(), false);
    assert.equal(unrelated.isClosed(), false);
    assert.equal(agent.isClosed(), false);
    await unrelated.close();
    assert.deepEqual(await unrelatedRead, { type: "closed" });
    await agent.close();
    assert.deepEqual(await allRead, { type: "closed" });
    assert.equal(all.isClosed(), true);
    await all.close();
  } finally {
    await agent.close();
  }
}
