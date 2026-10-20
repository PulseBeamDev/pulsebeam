import assert from "node:assert/strict";
import { test } from "node:test";
import { TopicRegistry } from "../../unit-javascript/topic.js";

function message(value, sequence, identity = {}) {
  return {
    type: "message",
    dataTrackId: "track-a",
    participantId: "alice",
    topic: "chat",
    sequence,
    value,
    ...identity,
  };
}

function fixture(send = async () => {}) {
  const listeners = new Set();
  const sends = [];
  const sequences = new Map();
  let registrations = [];
  const registry = new TopicRegistry(
    {
      subscribeEvents(listener) {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
    },
    async (name, mode, bytes) => {
      sends.push({ name, mode, bytes });
      await send();
    },
    (next) => (registrations = next),
  );
  return {
    registry,
    listeners,
    sends,
    get registrations() {
      return registrations;
    },
    event(event) {
      for (const listener of [...listeners]) listener(event);
    },
    emit(topic, mode, value, identity) {
      this.bytes(
        topic,
        mode,
        new TextEncoder().encode(JSON.stringify(value)),
        identity,
      );
    },
    bytes(topic, mode, payload, identity = {}) {
      const dataTrackId = identity.dataTrackId ?? "track-a";
      const sequence = identity.sequence ?? sequences.get(dataTrackId) ?? 0n;
      sequences.set(dataTrackId, sequence + 1n);
      this.event({
        type: "topic-message",
        topic,
        mode,
        payload,
        participantId: "alice",
        ...identity,
        dataTrackId,
        sequence,
      });
    },
  };
}

for (const mode of ["latest", "lossy", "ordered"]) {
  test(`${mode} preserves metadata, serializes UTF-8, and aggregates interest`, async () => {
    const f = fixture();
    const topic = f.registry.topic("chat", { mode });
    assert.equal(topic, f.registry.topic("chat", { mode }));
    assert.deepEqual(f.registrations, []);
    const first = topic.subscribe()[Symbol.asyncIterator]();
    const second = topic.subscribe()[Symbol.asyncIterator]();
    assert.deepEqual(f.registrations, [
      { name: "chat", mode, subscribe: true, publish: false },
    ]);
    const next = first.next();
    f.emit("other", mode, 0);
    f.emit("chat", mode === "latest" ? "ordered" : "latest", 0);
    const sequence = 2n ** 63n + 7n;
    f.emit("chat", mode, { text: "héllo" }, { sequence });
    const result = await next;
    assert.deepEqual(result, {
      value: message({ text: "héllo" }, sequence),
      done: false,
    });
    assert.ok(Object.isFrozen(result.value));
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

test("same-name modes stay distinct and ordered gaps are inline in listener arrival order", () => {
  const f = fixture();
  const received = [];
  const errors = [];
  const stops = ["latest", "lossy", "ordered"].map((mode) =>
    f.registry.listen(
      "chat",
      { mode },
      {
        onEvent: (event) => received.push({ mode, event }),
        onError: (error) => errors.push(error),
      },
    ),
  );
  assert.equal(f.registrations.length, 3);
  f.emit("chat", "ordered", "before", { sequence: 0n });
  const gap = {
    dataTrackId: "track-a",
    participantId: "alice",
    topic: "chat",
    expectedSequence: 1n,
    availableSequence: 4n,
  };
  f.event({ type: "topic-recovery-gap", ...gap });
  f.emit("chat", "ordered", "after", { sequence: 4n });
  f.emit("chat", "lossy", "a");
  f.emit("chat", "lossy", "b");
  assert.deepEqual(received, [
    { mode: "ordered", event: message("before", 0n) },
    { mode: "ordered", event: { type: "gap", ...gap } },
    { mode: "ordered", event: message("after", 4n) },
    { mode: "lossy", event: message("a", 5n) },
    { mode: "lossy", event: message("b", 6n) },
  ]);
  f.event({
    type: "topic-channel-failed",
    direction: "subscribe",
    topic: "chat",
    mode: "ordered",
    message: "closed",
  });
  assert.equal(errors[0].code, "channel-failed");
  f.emit("chat", "ordered", "fenced");
  assert.equal(received.length, 5);
  for (const stop of stops) {
    stop();
    stop();
  }
  assert.equal(f.listeners.size, 0);
  assert.deepEqual(f.registrations, []);
});

test("ordered overflow compacts missing messages into an inline gap before increasing delivery", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("chat", { mode: "ordered" })
    .subscribe()
    [Symbol.asyncIterator]();
  for (let value = 0; value < 300; value++) f.emit("chat", "ordered", value);
  assert.deepEqual((await iterator.next()).value, {
    type: "gap",
    topic: "chat",
    dataTrackId: "track-a",
    participantId: "alice",
    expectedSequence: 0n,
    availableSequence: 45n,
  });
  for (let value = 45; value < 300; value++)
    assert.deepEqual(
      (await iterator.next()).value,
      message(value, BigInt(value)),
    );
  const pending = iterator.next();
  await iterator.return();
  assert.equal((await pending).done, true);
  assert.equal(f.listeners.size, 0);
  assert.deepEqual(f.registrations, []);
});

test("ordered overflow keeps an explicit contiguous cursor independently for each track", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("chat", { mode: "ordered" })
    .subscribe()
    [Symbol.asyncIterator]();
  for (let sequence = 0n; sequence < 300n; sequence++)
    for (const participantId of ["alice", "bob"])
      f.emit("chat", "ordered", Number(sequence), {
        participantId,
        dataTrackId: participantId,
        sequence,
      });
  const cursors = new Map([
    ["alice", 0n],
    ["bob", 0n],
  ]);
  for (let i = 0; i < 256; i++) {
    const event = (await iterator.next()).value;
    assert.equal(event.dataTrackId, event.participantId);
    assert.equal(
      event.type === "gap" ? event.expectedSequence : event.sequence,
      cursors.get(event.dataTrackId),
    );
    cursors.set(
      event.dataTrackId,
      event.type === "gap" ? event.availableSequence : event.sequence + 1n,
    );
  }
  assert.deepEqual([...cursors.values()], [300n, 300n]);
  await iterator.return();
});

test("latest coalesces only per track and resolves pending reads without delaying them", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("chat", { mode: "latest" })
    .subscribe()
    [Symbol.asyncIterator]();
  const first = iterator.next();
  const second = iterator.next();
  for (let value = 0; value < 300; value++) {
    f.emit("chat", "latest", value);
    f.emit("chat", "latest", value, {
      dataTrackId: "track-b",
      participantId: "bob",
    });
  }
  assert.deepEqual((await first).value, message(0, 0n));
  assert.deepEqual(
    (await second).value,
    message(0, 0n, { dataTrackId: "track-b", participantId: "bob" }),
  );
  assert.deepEqual((await iterator.next()).value, message(299, 299n));
  assert.deepEqual(
    (await iterator.next()).value,
    message(299, 299n, { dataTrackId: "track-b", participantId: "bob" }),
  );
  await iterator.return();
});

test("lossy keeps distinct messages below capacity and discards oldest values at its bound", async () => {
  const f = fixture();
  const iterator = f.registry
    .topic("chat", { mode: "lossy" })
    .subscribe()
    [Symbol.asyncIterator]();
  for (let value = 0; value < 300; value++) f.emit("chat", "lossy", value);
  for (let value = 44; value < 300; value++)
    assert.deepEqual(
      (await iterator.next()).value,
      message(value, BigInt(value)),
    );
  await iterator.return();
});

for (const mode of ["latest", "ordered"]) {
  test(`${mode} fails explicitly when its finite buffer cannot represent every track`, async () => {
    const f = fixture();
    const iterator = f.registry
      .topic("chat", { mode })
      .subscribe()
      [Symbol.asyncIterator]();
    for (let track = 0; track < 257; track++)
      f.emit("chat", mode, track, { dataTrackId: `track-${track}` });
    await assert.rejects(iterator.next(), RangeError);
    assert.equal(f.listeners.size, 0);
    assert.deepEqual(f.registrations, []);
  });
}

test("pending reads and publish admissions are bounded and settle completely", async () => {
  let release;
  const gate = new Promise((resolve) => (release = resolve));
  const f = fixture(() => gate);
  const topic = f.registry.topic("chat", { mode: "ordered" });
  const iterator = topic.subscribe()[Symbol.asyncIterator]();
  const reads = Promise.allSettled(
    Array.from({ length: 256 }, () => iterator.next()),
  );
  await assert.rejects(iterator.next(), /pending reads/);
  assert.ok((await reads).every((result) => result.status === "rejected"));
  assert.equal(f.listeners.size, 0);
  const publishes = Array.from({ length: 256 }, (_, value) =>
    topic.publish(value),
  );
  await assert.rejects(topic.publish(256), /admission queue/);
  release();
  await Promise.all(publishes);
  assert.equal(f.sends.length, 256);
  f.registry.close();
});

test("abort, iterator throw, and close release listeners and settle every pending read", async () => {
  const f = fixture();
  const topic = f.registry.topic("lifetime", { mode: "ordered" });
  const controller = new AbortController();
  const iterator = topic
    .subscribe({ signal: controller.signal })
    [Symbol.asyncIterator]();
  const pending = [iterator.next(), iterator.next()];
  controller.abort();
  assert.ok((await Promise.all(pending)).every((result) => result.done));
  assert.equal(f.listeners.size, 0);
  assert.deepEqual(f.registrations, []);
  assert.equal(
    (
      await topic
        .subscribe({ signal: controller.signal })
        [Symbol.asyncIterator]()
        .next()
    ).done,
    true,
  );
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
  assert.throws(() => f.registry.topic("new", { mode: "ordered" }), /closed/);
  assert.deepEqual(f.sends, []);
});

for (const payload of [new Uint8Array([0xff]), new TextEncoder().encode("{")]) {
  test(`malformed payload ${payload} rejects and fences both iterator and listener`, async () => {
    const f = fixture();
    const errors = [];
    const stop = f.registry.listen(
      "bad",
      { mode: "ordered" },
      {
        onEvent: () => assert.fail("malformed value delivered"),
        onError: (error) => errors.push(error),
      },
    );
    const iterator = f.registry
      .topic("bad", { mode: "ordered" })
      .subscribe()
      [Symbol.asyncIterator]();
    const pending = assert.rejects(iterator.next());
    f.bytes("bad", "ordered", payload);
    await pending;
    assert.equal(errors[0].code, "decode-failed");
    stop();
    assert.equal(f.listeners.size, 0);
    assert.deepEqual(f.registrations, []);
    await assert.rejects(iterator.next());
  });
}

test("invalid values and obsolete options cannot register interest; byte limit is UTF-8", async () => {
  const f = fixture();
  for (const name of [undefined, null, "", "bad name", "é", "x".repeat(86)])
    assert.throws(() => f.registry.topic(name, { mode: "ordered" }), TypeError);
  for (const mode of ["reliable", "unreliable", undefined])
    assert.throws(() => f.registry.topic("ok", { mode }), TypeError);
  assert.throws(
    () => f.registry.topic("ok", { mode: "ordered", publisherId: "alice" }),
    TypeError,
  );
  const topic = f.registry.topic("valid_-09", { mode: "ordered" });
  assert.throws(() => topic.subscribe({ publisherId: "alice" }), TypeError);
  assert.throws(
    () => f.registry.listen("ok", { mode: "ordered" }, { onMessage() {} }),
    TypeError,
  );
  assert.throws(
    () =>
      f.registry.listen(
        "ok",
        { mode: "ordered" },
        { onEvent() {}, onGap() {} },
      ),
    TypeError,
  );
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
