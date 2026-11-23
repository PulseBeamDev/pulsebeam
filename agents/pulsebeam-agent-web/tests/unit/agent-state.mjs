import assert from "node:assert/strict";
import { test } from "node:test";
import {
  copyConfig,
  copyState,
  emptySnapshot,
  freezeTopicSnapshot,
  validatePlayoutDelay,
  validateToken,
} from "../../unit-javascript/agent-state.js";

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

test("credentials are bounded before runtime creation and renewal", () => {
  const token = "t".repeat(8192);
  assert.equal(
    copyConfig({ endpoint: "https://example.test", token }).token,
    token,
  );
  assert.doesNotThrow(() => validateToken(token));
  for (const invalid of [
    "t".repeat(8193),
    "",
    "bad\nheader",
    "é",
    null,
    5,
    {},
    undefined,
    true,
  ]) {
    assert.throws(
      () => validateToken(invalid),
      /authorization token is invalid/,
    );
    assert.throws(
      () => copyConfig({ endpoint: "https://example.test", token: invalid }),
      /authorization token is invalid/,
    );
  }
});

test("desired revisions copy nested policy and omission withdraws complete intent", () => {
  const input = {
    connected: true,
    publications: [
      { slot: "v0", kind: "video", trackId: "camera", active: true },
    ],
    video: [
      {
        slot: 0,
        selector: { room: "room", participantId: "alice", trackId: "camera" },
        height: 180,
        playoutDelay: { mode: "fixed", minMs: 10, maxMs: 20 },
      },
    ],
    audio: {
      pinned: ["voice"],
      selected: [{ room: "room", participantId: "alice", trackId: "voice" }],
    },
    topics: [{ name: "chat", mode: "ordered", publish: true }],
  };
  const state = copyState(input);
  input.video[0].selector.trackId = "changed";
  input.video[0].playoutDelay.minMs = 99;
  input.audio.pinned.push("other");
  input.publications[0].active = false;
  input.topics[0].publish = false;
  assert.equal(state.video[0].selector.trackId, "camera");
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
  assert.deepEqual(withdrawn.audio.selected, []);
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
  assert.equal(initial.joinRevision, 0n);
  assert.equal(initial.credentialRevision, 0n);
  assert.equal(initial.acceptedCredentialRevision, null);
  assert.equal(initial.credentialOutcome, null);
  assert.equal(initial.desiredRoom, null);
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
