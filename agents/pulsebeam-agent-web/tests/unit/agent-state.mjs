import assert from "node:assert/strict";
import { test } from "node:test";
import {
  copyConfig,
  copyState,
  emptySnapshot,
  freezeTopicSnapshot,
  validatePlayoutDelay,
} from "../../unit-javascript/agent-state.js";

test("configuration owns immutable capacities and preserves explicit zero", () => {
  const input = {
    endpoint: "https://example.test",
    token: "credential",
    topology: { localVideos: 0 },
  };
  const config = copyConfig(input);
  input.topology.localVideos = 8;
  assert.deepEqual(config.topology, {
    localVideos: 0,
    localAudios: 2,
    remoteVideos: 16,
    remoteAudios: 8,
  });
  assert.equal(config.logLevel, "warn");
  assert.ok(Object.isFrozen(config) && Object.isFrozen(config.topology));
  assert.equal(
    copyConfig({ ...input, logging: { level: "off" } }).logLevel,
    "off",
  );
});

test("desired revisions copy nested policy and omission withdraws complete intent", () => {
  const input = {
    connected: true,
    publications: [{ slot: "v0", label: "camera", active: true }],
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
  assert.equal(state.topics[0].publish, true);
  assert.ok(Object.isFrozen(state.video[0].selector));
  const withdrawn = copyState({ connected: false });
  assert.deepEqual(withdrawn.publications, []);
  assert.deepEqual(withdrawn.video, []);
  assert.deepEqual(withdrawn.topics, []);
  assert.deepEqual(withdrawn.audio.pinned, []);
  assert.equal(withdrawn.audio.automatic, true);
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
