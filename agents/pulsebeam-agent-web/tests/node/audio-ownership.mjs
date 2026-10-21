import assert from "node:assert/strict";
import { Track } from "./platform.mjs";
import {
  RemoteCatalog,
  attachRemoteAudio,
} from "../../node_assets/dist/remote-catalog.js";
import {
  ResourceOwners,
  ResourceOwnershipError,
} from "../../node_assets/dist/resources.js";
import { DataRegistry } from "../../node_assets/dist/data.js";

// Real core admission and host attachment, with controlled decoded media and audio I/O.
export async function audioOwnershipContract(runtime) {
  const previous = {
    AudioContext: globalThis.AudioContext,
    document: globalThis.document,
  };
  const contexts = [];
  const decoders = [];
  let blocked = true;
  class Context {
    state = "suspended";
    destination = {};
    routes = new Set();
    constructor() {
      contexts.push(this);
    }
    async resume() {
      if (blocked) throw new Error("controlled autoplay rejection");
      this.state = "running";
    }
    async close() {
      this.state = "closed";
    }
    createMediaStreamSource() {
      return { connect() {}, disconnect() {} };
    }
    createAnalyser() {
      const route = {
        fftSize: 16,
        frequencyBinCount: 8,
        connect: () => this.routes.add(route),
        disconnect: () => this.routes.delete(route),
      };
      return route;
    }
  }
  globalThis.AudioContext = Context;
  globalThis.document = {
    createElement: () => {
      const decoder = {
        srcObject: null,
        paused: true,
        async play() {
          this.paused = false;
        },
        pause() {
          this.paused = true;
        },
      };
      decoders.push(decoder);
      return decoder;
    },
  };
  const borrowed = new Track("audio");
  const snapshot = {
    connection: "connected",
    catalog: { participants: [], publications: [], tracks: [] },
    mapping: { audio: [{ receiverIndex: 0, publicationId: "voice" }] },
    tracks: { voice: { media: borrowed } },
  };
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  const owners = new ResourceOwners(() => {});
  const catalog = new RemoteCatalog(
    agent,
    new DataRegistry(agent),
    0,
    () => {},
    owners,
    "off",
  );
  try {
    catalog.update(snapshot);
    const failures = [];
    const first = attachRemoteAudio(agent, {
      onPlaybackError: (error) => failures.push(error),
    });
    assert.equal(
      contexts.length,
      0,
      "uncommitted attachment cannot allocate a graph",
    );
    owners.activate(runtime);
    await first.ready;
    for (
      let turn = 0;
      turn < 1000 &&
      !failures.some(
        (error) => error.message === "controlled autoplay rejection",
      );
      turn++
    )
      await Promise.resolve();
    assert.ok(
      failures.some(
        (error) => error.message === "controlled autoplay rejection",
      ),
    );
    assert.equal(contexts.length, 1);
    assert.equal(contexts[0].routes.size, 1);
    const duplicate = attachRemoteAudio(agent);
    await assert.rejects(
      duplicate.ready,
      (error) =>
        error instanceof ResourceOwnershipError && error.code === "ownership",
    );
    await duplicate.close();
    assert.equal(contexts.length, 1, "declined owner cannot allocate a graph");
    assert.equal(
      contexts[0].routes.size,
      1,
      "declined owner cannot stop the committed route",
    );
    blocked = false;
    await failures[0].retry();
    assert.equal(contexts[0].state, "running");
    await first.close();
    assert.equal(contexts[0].routes.size, 0);
    assert.equal(contexts[0].state, "closed");
    assert.equal(decoders[0].srcObject, null);
    await failures[0].retry();
    assert.equal(
      contexts.length,
      1,
      "closed owner's retry cannot recreate playback",
    );
    const replacement = attachRemoteAudio(agent);
    await replacement.ready;
    assert.equal(contexts.length, 2, "released audio ownership is reusable");
    owners.deactivate();
    assert.equal(contexts[1].state, "closed");
    assert.equal(contexts[1].routes.size, 0);
    await replacement.close();
    assert.equal(borrowed.readyState, "live");
  } finally {
    catalog.close();
    Object.assign(globalThis, previous);
    borrowed.stop();
  }
}
