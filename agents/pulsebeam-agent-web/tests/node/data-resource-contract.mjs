import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";

export async function dataResourceContract(pulsebeam) {
  const config = {
    endpoint: "http://localhost",
    token: "owned-binary",
    logging: { level: "off" },
  };
  const agent = new pulsebeam.Agent(config);
  const until = async (condition, label) => {
    for (let step = 0; step < 1000; step++) {
      if (condition()) return;
      await setImmediate();
    }
    assert.fail(label);
  };
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
    assert.deepEqual(agent.snapshot().topics.publishers, []);
    assert.deepEqual(agent.snapshot().topics.subscribers, []);
    const publication = local.publish();
    await assert.rejects(publication.send(Uint8Array.of(255)), (error) => {
      assert.ok(error instanceof pulsebeam.OperationError);
      assert.equal(error.info.code, pulsebeam.ErrorCode.NotReady);
      assert.equal(Object.isFrozen(error.info), true);
      return true;
    });

    const longLabel = "é".repeat(128);
    const longNamed = agent.data(longLabel, "ordered");
    assert.equal(
      longNamed.label,
      longLabel,
      "local track labels retain their existing Core limit",
    );
    for (const label of ["", "x".repeat(65), "é".repeat(33)]) {
      assert.throws(
        () =>
          agent
            .room("room")
            .participant("alice")
            .data(label, "ordered")
            .subscribe(),
        (error) => {
          assert.ok(error instanceof pulsebeam.OperationError);
          assert.equal(error.info.code, pulsebeam.ErrorCode.InvalidArgument);
          return true;
        },
      );
    }
    await until(
      () => agent.snapshot().topics.subscribers.length === 0,
      "validation failures and closed receivers release their interest",
    );

    const all = await agent
      .room("room")
      .participant("carol")
      .data("events", "ordered")
      .subscribe();
    const scoped = alice.subscribe();
    const unrelated = bob.subscribe();
    await until(
      () => agent.snapshot().topics.subscribers.length === 3,
      "distinct pinned sources retain separate Core-owned interests",
    );
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
    assert.equal(agent.snapshot().topics.subscribers.length, 2);
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
