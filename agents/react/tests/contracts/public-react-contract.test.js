import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import TestRenderer, { act } from "react-test-renderer";
import { AgentProvider, useAgent } from "../../dist/index.js";

globalThis.IS_REACT_ACT_ENVIRONMENT = true;

const disconnected = Object.freeze({
  version: 0,
  desiredRevision: 0,
  connection: "disconnected",
  generation: null,
  participantId: null,
  participants: Object.freeze([]),
  publications: Object.freeze([]),
  video: Object.freeze([]),
  audio: Object.freeze([]),
  tracks: Object.freeze({}),
  topics: Object.freeze({ publishers: [], subscribers: [], acceptedSends: 0, droppedSends: 0, deliveredMessages: 0, resynchronizations: 0, channelFailures: 0 }),
  failure: null,
});

class FakeAgent {
  snapshot;
  listeners = new Set();
  states = [];
  replacedTracks = [];
  muted = [];
  reconnectCalls = 0;
  topicSends = [];
  eventListeners = new Set();
  unsubscribeCalls = 0;
  closeCalls = 0;

  constructor(snapshot = disconnected) {
    this.snapshot = snapshot;
  }

  getSnapshot = () => this.snapshot;

  subscribe = (listener) => {
    this.listeners.add(listener);
    return () => {
      if (this.listeners.delete(listener)) {
        this.unsubscribeCalls += 1;
      }
    };
  };

  setState(state) {
    this.states.push(state);
  }

  replaceLocalTrack(slot, track, config) {
    this.replacedTracks.push([slot, track, config]);
    return Promise.resolve();
  }

  setLocalMuted(slot, muted) {
    this.muted.push([slot, muted]);
    return Promise.resolve();
  }

  reconnect() {
    this.reconnectCalls += 1;
  }

  sendTopic(name, mode, payload) {
    this.topicSends.push([name, mode, payload]);
  }

  subscribeEvents(listener) {
    this.eventListeners.add(listener);
    return () => this.eventListeners.delete(listener);
  }

  close() {
    this.closeCalls += 1;
  }

  emit(snapshot = this.snapshot) {
    this.snapshot = snapshot;
    for (const listener of this.listeners) {
      listener();
    }
  }
}

function Probe({ onRender }) {
  onRender(useAgent());
  return null;
}

function provided(agent, onRender) {
  return createElement(
    AgentProvider,
    { agent },
    createElement(Probe, { onRender }),
  );
}

test("renders snapshots, rerenders on updates, and preserves stable results", () => {
  const agent = new FakeAgent();
  const results = [];
  let renderer;

  act(() => {
    renderer = TestRenderer.create(provided(agent, (result) => results.push(result)));
  });

  assert.equal(results.length, 1);
  assert.equal(results[0].connection, "disconnected");
  assert.equal(results[0].participantId, null);
  assert.deepEqual(results[0].tracks, {});

  act(() => agent.emit());
  assert.equal(results.length, 1);

  act(() => renderer.update(provided(agent, (result) => results.push(result))));
  assert.equal(results.length, 2);
  assert.strictEqual(results[1], results[0]);

  const connected = Object.freeze({
    ...disconnected,
    version: 1,
    connection: "connected",
    participantId: "participant-1",
    participants: Object.freeze([{ id: "participant-1" }]),
    publications: Object.freeze([{ id: "publication-1", participantId: "participant-1", kind: "video" }]),
    video: Object.freeze([{ trackId: "track-1", mid: "0", paused: false }]),
    tracks: Object.freeze({ "publication-1": { kind: "video", publicationId: "publication-1", participantId: "participant-1", mid: "0", media: null, paused: false } }),
    failure: Object.freeze({ class: "runtime", message: "media failed" }),
  });
  act(() => agent.emit(connected));
  assert.equal(results.length, 3);
  assert.notStrictEqual(results[2], results[1]);
  assert.equal(results[2].connection, "connected");
  assert.equal(results[2].participantId, "participant-1");
  assert.deepEqual(results[2].participants, [{ id: "participant-1" }]);
  assert.equal(results[2].tracks["publication-1"].kind, "video");
  assert.equal(results[2].failure.message, "media failed");

  const state = { connected: false };
  results[2].setState(state);
  assert.deepEqual(agent.states, [state]);

  act(() => renderer.unmount());
});

test("replacement resubscribes, delegates to the current agent, and never closes", async () => {
  const first = new FakeAgent();
  const second = new FakeAgent(
    Object.freeze({
      ...disconnected,
      connection: "joining",
    }),
  );
  const results = [];
  const onRender = (result) => results.push(result);
  let renderer;

  act(() => {
    renderer = TestRenderer.create(provided(first, onRender));
  });
  assert.equal(first.listeners.size, 1);

  act(() => renderer.update(provided(second, onRender)));
  assert.equal(first.listeners.size, 0);
  assert.equal(first.unsubscribeCalls, 1);
  assert.equal(second.listeners.size, 1);
  assert.equal(results.at(-1).connection, "joining");

  const state = { connected: true };
  results.at(-1).setState(state);
  assert.deepEqual(first.states, []);
  assert.deepEqual(second.states, [state]);

  const track = {};
  const config = { contentHint: "motion" };
  await results.at(-1).replaceLocalTrack("camera", track, config);
  await results.at(-1).setLocalMuted("mic", true);
  results.at(-1).reconnect();
  const payload = new Uint8Array([1]);
  results.at(-1).sendTopic("chat", "ordered", payload);
  assert.deepEqual(second.replacedTracks, [["camera", track, config]]);
  assert.deepEqual(second.muted, [["mic", true]]);
  assert.equal(second.reconnectCalls, 1);
  assert.deepEqual(second.topicSends, [["chat", "ordered", payload]]);

  const received = [];
  const unsubscribeEvents = results.at(-1).subscribeEvents((event) => received.push(event));
  second.eventListeners.forEach((listener) => listener({ type: "topic-message", mode: "latest", topic: "reaction", publisherId: "participant-1", payload }));
  unsubscribeEvents();
  assert.equal(second.eventListeners.size, 0);
  assert.equal(received.length, 1);

  act(() => renderer.unmount());
  assert.equal(second.listeners.size, 0);
  assert.equal(second.unsubscribeCalls, 1);
  assert.equal(first.closeCalls, 0);
  assert.equal(second.closeCalls, 0);
});

test("fails deterministically without a provider", () => {
  function MissingProvider() {
    useAgent();
    return null;
  }

  const originalError = console.error;
  console.error = () => {};
  try {
    assert.throws(
      () => act(() => TestRenderer.create(createElement(MissingProvider))),
      { message: "useAgent requires AgentProvider" },
    );
  } finally {
    console.error = originalError;
  }
});
