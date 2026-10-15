import assert from "node:assert/strict";
import { test } from "node:test";
import { TopicRegistry } from "../../unit-javascript/topic.js";

function fixture() {
  const listeners = new Set();
  const sends = [];
  let registrations = [];
  const registry = new TopicRegistry(
    {
      subscribeEvents(listener) {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
    },
    async (name, mode, bytes) => sends.push({ name, mode, bytes }),
    (next) => (registrations = next),
  );
  return {
    registry,
    listeners,
    sends,
    get registrations() {
      return registrations;
    },
    emit(topic, mode, value) {
      this.bytes(topic, mode, new TextEncoder().encode(JSON.stringify(value)));
    },
    bytes(topic, mode, payload) {
      for (const listener of listeners)
        listener({ type: "topic-message", topic, mode, payload });
    },
  };
}

for (const [facade, mode] of [
  ["reliable", "ordered"],
  ["unreliable", "latest"],
]) {
  test(`${facade} converts mode, serializes UTF-8, and aggregates interest`, async () => {
    const f = fixture();
    const topic = f.registry.topic("chat", { mode: facade });
    assert.equal(topic, f.registry.topic("chat", { mode: facade }));
    assert.deepEqual(f.registrations, []);
    const first = topic.subscribe()[Symbol.asyncIterator]();
    const second = topic.subscribe()[Symbol.asyncIterator]();
    assert.deepEqual(f.registrations, [
      { name: "chat", mode, subscribe: true, publish: false },
    ]);
    const next = first.next();
    f.emit("other", mode, 0);
    f.emit("chat", mode === "latest" ? "ordered" : "latest", 0);
    f.emit("chat", mode, { text: "héllo" });
    assert.deepEqual(await next, { value: { text: "héllo" }, done: false });
    await topic.publish({ text: "héllo" });
    assert.deepEqual(f.sends, [
      {
        name: "chat",
        mode,
        bytes: new TextEncoder().encode('{"text":"héllo"}'),
      },
    ]);
    await first.return();
    assert.equal(f.registrations[0].subscribe, true);
    await second.return();
    assert.equal(f.listeners.size, 0);
    assert.deepEqual(f.registrations, [
      { name: "chat", mode, subscribe: false, publish: true },
    ]);
    f.registry.close();
  });
}

test("ordered overflow retains exactly the newest 256 values in order", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("queue", { mode: "reliable" })
    .subscribe()
    [Symbol.asyncIterator]();
  for (let value = 0; value < 300; value++) f.emit("queue", "ordered", value);
  for (let value = 44; value < 300; value++)
    assert.deepEqual(await iterator.next(), { value, done: false });
  const pending = iterator.next();
  await iterator.return();
  assert.equal((await pending).done, true);
  assert.equal(f.listeners.size, 0);
  assert.deepEqual(f.registrations, []);
});

test("latest coalesces queued values but resolves pending reads in arrival order", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("presence", { mode: "unreliable" })
    .subscribe()
    [Symbol.asyncIterator]();
  const first = iterator.next();
  const second = iterator.next();
  for (let value = 0; value < 300; value++) f.emit("presence", "latest", value);
  assert.equal((await first).value, 0);
  assert.equal((await second).value, 1);
  assert.equal((await iterator.next()).value, 299);
  await iterator.return();
});

test("abort, iterator throw, and close release listeners and settle every pending read", async () => {
  const f = fixture();
  const topic = f.registry.topic("lifetime", { mode: "reliable" });
  const controller = new AbortController();
  const iterator = topic
    .subscribe({ signal: controller.signal })
    [Symbol.asyncIterator]();
  const pending = [iterator.next(), iterator.next()];
  controller.abort();
  assert.ok((await Promise.all(pending)).every((result) => result.done));
  assert.equal(f.listeners.size, 0);
  assert.deepEqual(f.registrations, []);
  const aborted = topic
    .subscribe({ signal: controller.signal })
    [Symbol.asyncIterator]();
  assert.equal((await aborted.next()).done, true);
  assert.equal(f.listeners.size, 0);
  const thrown = topic.subscribe()[Symbol.asyncIterator]();
  const error = new Error("consumer failed");
  const rejection = assert.rejects(thrown.next(), error);
  await assert.rejects(thrown.throw(error), error);
  await rejection;
  assert.equal(f.listeners.size, 0);
  const closed = topic.subscribe()[Symbol.asyncIterator]();
  f.emit("lifetime", "ordered", "discard on close");
  const waiting = topic.subscribe()[Symbol.asyncIterator]().next();
  f.registry.close();
  f.registry.close();
  assert.equal((await closed.next()).done, true);
  assert.equal((await waiting).done, true);
  assert.equal(f.listeners.size, 0);
  await assert.rejects(topic.publish(1), /closed/);
  assert.throws(() => topic.subscribe(), /closed/);
  assert.throws(() => f.registry.topic("new", { mode: "reliable" }), /closed/);
  assert.deepEqual(f.sends, []);
});

for (const payload of [new Uint8Array([0xff]), new TextEncoder().encode("{")]) {
  test(`malformed payload ${payload} rejects and fences its subscription`, async () => {
    const f = fixture();
    const iterator = f.registry
      .topic("bad", { mode: "reliable" })
      .subscribe()
      [Symbol.asyncIterator]();
    const pending = assert.rejects(iterator.next());
    f.bytes("bad", "ordered", payload);
    await pending;
    assert.equal(f.listeners.size, 0);
    assert.deepEqual(f.registrations, []);
    await assert.rejects(iterator.next());
  });
}

test("invalid values cannot register publishing interest or send; byte limit is UTF-8", async () => {
  const f = fixture();
  for (const name of ["", "bad name", "é", "x".repeat(86)])
    assert.throws(
      () => f.registry.topic(name, { mode: "reliable" }),
      TypeError,
    );
  assert.throws(() => f.registry.topic("ok", { mode: "ordered" }), TypeError);
  const topic = f.registry.topic("valid_-09", { mode: "reliable" });
  const circular = {};
  circular.self = circular;
  for (const value of [undefined, () => {}, 1n, circular, "é".repeat(32768)])
    await assert.rejects(topic.publish(value));
  assert.deepEqual(f.registrations, []);
  assert.deepEqual(f.sends, []);
  await topic.publish("é".repeat(32767));
  assert.equal(f.sends[0].bytes.byteLength, 65536);
  f.registry.close();
});
