import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";

export async function dataResourceContract(pulsebeam) {
  const config = {
    endpoint: "http://localhost",
    token: "owned-binary",
    logging: { level: "off" },
  };
  const agent = await pulsebeam.createAgent(config);
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
    const other = await pulsebeam.createAgent(config);
    try {
      const otherLocal = other.data("events", "ordered");
      assert.notEqual(local, otherLocal);
      const pendingAdmission = otherLocal.receive();
      await other.close();
      const closedDuringAdmission = await pendingAdmission;
      assert.equal(closedDuringAdmission.isClosed(), true);
      assert.deepEqual(await closedDuringAdmission.next(), { type: "closed" });
      await closedDuringAdmission.close();
    } finally {
      await other.close();
    }
    const alice = agent.participant("alice").data("events", "ordered");
    const bob = agent.participant("bob").data("events", "ordered");
    assert.equal(alice, agent.participant("alice").data("events", "ordered"));
    assert.notEqual(alice, bob);
    assert.equal(alice.id, null);
    assert.equal(alice.active, false);
    assert.deepEqual(agent.snapshot().topics.publishers, []);
    assert.deepEqual(agent.snapshot().topics.subscribers, []);
    await assert.rejects(local.send(Uint8Array.of(255)), {
      code: "not-connected",
    });

    const longLabel = "é".repeat(128);
    const longNamed = await agent.data(longLabel, "ordered").receive();
    await until(
      () =>
        agent
          .snapshot()
          .topics.subscribers.some(({ name }) => name === longLabel),
      "Core-valid 256-byte label was rejected by the facade",
    );
    await longNamed.close();
    for (const label of ["", "x".repeat(257), "é".repeat(129)]) {
      await assert.rejects(agent.data(label, "ordered").receive(), {
        code: "validation",
      });
    }
    await until(
      () => agent.snapshot().topics.subscribers.length === 0,
      "validation failures and closed receivers release their interest",
    );

    const all = await local.receive();
    const scoped = await alice.receive();
    const unrelated = await bob.receive();
    await until(
      () => agent.snapshot().topics.subscribers.length === 1,
      "independent receivers share one Core-owned channel interest",
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
    assert.equal(agent.snapshot().topics.subscribers.length, 1);
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
