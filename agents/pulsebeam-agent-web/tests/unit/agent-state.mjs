import assert from "node:assert/strict";
import { test } from "node:test";
import {
  copyConfig,
  copyState,
  emptySnapshot,
  freezeTopicSnapshot,
  validatePlayoutDelay,
} from "../../unit-javascript/agent-state.js";

import {
  observeSnapshots,
  observeEvents,
} from "../../unit-javascript/observation.js";

test("state cursors are bounded, independently closed, and wake all pending reads", async () => {
  let released = 0;
  const initial = emptySnapshot("disconnected");
  const first = observeSnapshots(initial, false, () => released++);
  const sibling = observeSnapshots(initial, false, () => released++);
  const events = observeEvents(false, () => released++);
  assert.deepEqual(await first.stream.next(), {
    type: "snapshot",
    snapshot: initial,
  });
  const pending = first.stream.next();
  const queued = first.stream.next();
  await first.stream.close();
  await first.stream.close();
  assert.deepEqual(await pending, { type: "closed" });
  assert.deepEqual(await queued, { type: "closed" });
  assert.equal(released, 1);
  assert.equal(sibling.stream.isClosed(), false);
  for (let index = 0; index < 70; index++) sibling.publish(initial);
  assert.deepEqual(
    await sibling.stream.next(),
    { type: "snapshot", snapshot: initial },
    "initial snapshot is not lost when later updates overflow",
  );
  assert.deepEqual(await sibling.stream.next(), {
    type: "lagged",
    skipped: 6n,
  });
  assert.equal((await sibling.stream.next()).type, "snapshot");
  const retiring = sibling.stream.next();
  sibling.close();
  assert.deepEqual(
    await retiring,
    { type: "closed" },
    "retirement wins over queued data",
  );
  for (let index = 0; index < 70; index++)
    events.publish({ type: "state-change" });
  assert.deepEqual(await events.stream.next(), { type: "lagged", skipped: 6n });
  for (let index = 0; index < 64; index++)
    assert.equal((await events.stream.next()).type, "event");
  const pendingEvent = events.stream.next();
  events.close();
  assert.deepEqual(await pendingEvent, { type: "closed" });
  assert.equal(released, 3);
  assert.equal("observation" in first.stream, false);
});

test("configuration owns immutable connection and logging inputs", () => {
  const input = { endpoint: "https://example.test", token: "credential" };
  const config = copyConfig(input);
  input.endpoint = "https://other.test";
  input.token = "changed";
  assert.deepEqual(config, {
    endpoint: "https://example.test",
    token: "credential",
    logLevel: "warn",
  });
  assert.ok(Object.isFrozen(config));
  assert.equal(
    copyConfig({ ...input, logging: { level: "off" } }).logLevel,
    "off",
  );
});

test("desired revisions copy nested policy and omission withdraws complete intent", () => {
  const input = {
    connected: true,
    publications: [
      { slot: "v0", kind: "video", label: "camera", active: true },
    ],
    video: [
      {
        slot: 0,
        selector: { participantId: "alice", label: "camera" },
        height: 180,
        playoutDelay: { mode: "fixed", minMs: 10, maxMs: 20 },
      },
    ],
    audio: {
      automatic: false,
      pinned: ["voice"],
      selected: [{ participantId: "alice", label: "voice" }],
    },
    topics: [{ name: "chat", mode: "ordered", publish: true }],
  };
  const state = copyState(input);
  input.video[0].selector.label = "changed";
  input.video[0].playoutDelay.minMs = 99;
  input.audio.pinned.push("other");
  input.publications[0].active = false;
  input.topics[0].publish = false;
  assert.equal(state.video[0].selector.label, "camera");
  assert.equal(state.video[0].playoutDelay.minMs, 10);
  assert.deepEqual(state.audio.pinned, ["voice"]);
  assert.equal(state.publications[0].active, true);
  assert.equal(state.publications[0].kind, "video");
  assert.equal(state.topics[0].publish, true);
  assert.ok(Object.isFrozen(state.video[0].selector));
  const withdrawn = copyState({ connected: false });
  assert.deepEqual(withdrawn.publications, []);
  assert.deepEqual(withdrawn.video, []);
  assert.deepEqual(withdrawn.topics, []);
  assert.deepEqual(withdrawn.audio.pinned, []);
  assert.equal(withdrawn.audio.automatic, false);
  validatePlayoutDelay(state);
  for (const minMs of [-1, NaN, 0.5, 2 ** 32])
    assert.throws(
      () =>
        validatePlayoutDelay({
          connected: true,
          video: [{ playoutDelay: { mode: "fixed", minMs, maxMs: 20 } }],
        }),
      RangeError,
    );
});

test("empty and runtime topic snapshots cannot be mutated through retained input", () => {
  const initial = emptySnapshot("disconnected");
  assert.equal(initial.connection, "disconnected");
  assert.equal(initial.failure, null);
  assert.deepEqual(initial.tracks, {});
  assert.ok(
    Object.isFrozen(initial) && Object.isFrozen(initial.catalog.participants),
  );
  const input = {
    ...initial.topics,
    publishers: [{ name: "chat", connected: true }],
  };
  const topics = freezeTopicSnapshot(input);
  input.publishers[0].connected = false;
  assert.equal(topics.publishers[0].connected, true);
  assert.ok(Object.isFrozen(topics.publishers[0]));
});
