import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import TestRenderer, { act } from "react-test-renderer";
import { AgentProvider, useAgent } from "../../dist/index.js";

globalThis.IS_REACT_ACT_ENVIRONMENT = true;

const disconnected = Object.freeze({
  connection: "disconnected",
  participantId: null,
  tracks: Object.freeze([]),
});

class FakeAgent {
  snapshot;
  listeners = new Set();
  states = [];
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
  assert.deepEqual(results[0].tracks, []);

  act(() => agent.emit());
  assert.equal(results.length, 1);

  act(() => renderer.update(provided(agent, (result) => results.push(result))));
  assert.equal(results.length, 2);
  assert.strictEqual(results[1], results[0]);

  const connected = Object.freeze({
    connection: "connected",
    participantId: "participant-1",
    tracks: Object.freeze([]),
  });
  act(() => agent.emit(connected));
  assert.equal(results.length, 3);
  assert.notStrictEqual(results[2], results[1]);
  assert.equal(results[2].connection, "connected");
  assert.equal(results[2].participantId, "participant-1");

  const state = { connection: null };
  results[2].setState(state);
  assert.deepEqual(agent.states, [state]);

  act(() => renderer.unmount());
});

test("replacement resubscribes, delegates to the current agent, and never closes", () => {
  const first = new FakeAgent();
  const second = new FakeAgent(
    Object.freeze({
      connection: "connecting",
      participantId: null,
      tracks: Object.freeze([]),
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
  assert.equal(results.at(-1).connection, "connecting");

  const state = { connection: { roomId: "room", token: "token" } };
  results.at(-1).setState(state);
  assert.deepEqual(first.states, []);
  assert.deepEqual(second.states, [state]);

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
