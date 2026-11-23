import assert from "node:assert/strict";
import { test } from "node:test";
import {
  RemoteCatalog,
  attachSubscribedVideo,
  videoAttachmentConfiguration,
} from "../../unit-javascript/remote-catalog.js";
import { DataRegistry } from "../../unit-javascript/data.js";
import { attachMedia } from "../../unit-javascript/media-attachment.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";
import {
  Video,
  Resize,
  Intersection,
  Mutation,
  document,
  viewport,
  queries,
} from "./media-platform.mjs";

test("production attachment reports controlled geometry and visibility to core", async () => {
  const snapshot = {
    connection: "disconnected",
    catalog: {
      participants: [{ participantId: "alice" }],
      publications: [
        {
          id: "camera",
          kind: "video",
          participantId: "alice",
          trackId: "camera",
        },
      ],
    },
    mapping: { video: [], audio: [] },
    tracks: {},
  };
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  let demand = [];
  let nextOwner = 1n;
  const observations = new Map();
  const pending = new Map();
  const owners = new ResourceOwners(() => {});
  owners.activate({
    video_subscription: () => ({
      state: () => ({ observationRevision: 1n }),
      updates() {
        let first = true;
        let resolve;
        return {
          next() {
            if (first) {
              first = false;
              return Promise.resolve({ observationRevision: 1n });
            }
            return new Promise((done) => {
              resolve = done;
            });
          },
          close() {
            resolve?.(undefined);
          },
        };
      },
    }),
    async observe_media(owner, _source, signal) {
      return new Promise((_resolve, reject) => {
        const key = {};
        const cancel = () =>
          finish(
            Object.assign(new Error("closed"), { pulsebeamCode: "closed" }),
          );
        const finish = (error) => {
          signal?.removeEventListener("abort", cancel);
          pending.delete(key);
          reject(error);
        };
        pending.set(key, { owner, finish });
        signal?.addEventListener("abort", cancel, { once: true });
        if (signal?.aborted) cancel();
      });
    },
    async resource(command) {
      return this.resource_now(command);
    },
    resource_now(command) {
      if (command.op === "claimVideo") return { owner: nextOwner++ };
      if (command.op === "observeVideo") {
        observations.set(command.owner, command.observation);
        demand = [...observations.values()].filter(({ visible }) => visible);
        return { type: "updated" };
      }
      if (command.op === "release") {
        for (const reader of pending.values()) {
          if (reader.owner === command.owner)
            reader.finish(
              Object.assign(new Error("closed"), { pulsebeamCode: "closed" }),
            );
        }
        observations.delete(command.owner);
        demand = [...observations.values()].filter(({ visible }) => visible);
        return { type: "released" };
      }
      throw new Error(`unexpected resource command ${command.op}`);
    },
  });
  snapshot.catalog.tracks = snapshot.catalog.publications;
  const catalog = new RemoteCatalog(agent, new DataRegistry(agent), owners);
  catalog.update(snapshot);
  const handle = catalog.room("room").participant("alice").video("camera");
  const parent = new Video();
  const small = new Video();
  small.parentElement = parent;
  const first = attachSubscribedVideo(
    handle.subscribe(videoAttachmentConfiguration({ minHeight: 720 })),
    small,
  );
  await first.ready;
  assert.equal(demand[0].height, 240);
  const large = new Video();
  large.rect.height = 230;
  const second = attachSubscribedVideo(handle.subscribe(), large);
  await second.ready;
  assert.equal(demand.length, 2);
  assert.equal(demand[1].height, 460);
  await second.close();
  assert.equal(demand[0].height, 240);
  small.rect.height = 200;
  Resize.notify();
  assert.equal(demand[0].height, 400);
  for (const [property, hidden, visible] of [
    ["opacity", "0", "1"],
    ["visibility", "hidden", "visible"],
    ["visibility", "collapse", "visible"],
    ["display", "none", "block"],
  ]) {
    parent.style[property] = hidden;
    Mutation.notify();
    assert.deepEqual(demand, [], property);
    parent.style[property] = visible;
    Mutation.notify();
    assert.equal(demand[0].height, 400);
  }
  small.rect.top = -500;
  viewport.dispatchEvent(new Event("scroll"));
  assert.deepEqual(demand, []);
  small.rect.top = -50;
  viewport.dispatchEvent(new Event("scroll"));
  assert.equal(
    demand[0].height,
    400,
    "partial visibility preserves full demand",
  );
  document.hidden = true;
  document.dispatchEvent(new Event("visibilitychange"));
  assert.deepEqual(demand, []);
  document.hidden = false;
  document.dispatchEvent(new Event("visibilitychange"));
  Intersection.notify([{ isIntersecting: false }]);
  assert.deepEqual(demand, []);
  Intersection.notify([{ isIntersecting: true }]);
  assert.equal(demand[0].height, 400);
  viewport.devicePixelRatio = 3;
  for (const query of [...queries]) query.dispatchEvent(new Event("change"));
  assert.equal(demand[0].height, 600);
  assert.equal(demand[0].minHeight, 720);
  await first.close();
  assert.equal(
    pending.size,
    0,
    "teardown cancels pending physical observation",
  );
  assert.deepEqual(demand, []);
  assert.equal(small.srcObject, null);
  Resize.notify();
  Mutation.notify();
  Intersection.notify([{ isIntersecting: true }]);
  viewport.dispatchEvent(new Event("resize"));
  assert.deepEqual(demand, [], "teardown cannot resurrect measured demand");
  viewport.devicePixelRatio = 2;
  const configuration = {
    visible: true,
    height: 100,
    minHeight: 0,
    minFps: 15,
    priority: 10,
    playoutDelay: { mode: "adaptive" },
  };
  const subscription = handle.subscribe(configuration);
  await subscription.configure(configuration);
  small.rect.height = 100;
  const fail = (error) => assert.fail(error);
  const borrowedSmall = attachMedia(
    subscription,
    "video",
    small,
    false,
    fail,
    fail,
  );
  const borrowedLarge = attachMedia(
    subscription,
    "video",
    large,
    false,
    fail,
    fail,
  );
  assert.equal(
    observations.size,
    1,
    "views borrow one existing reception owner",
  );
  assert.equal(
    demand[0].height,
    460,
    "one subscription aggregates its attached views",
  );
  await subscription.configure({ ...configuration, priority: 200 });
  assert.equal(
    demand[0].height,
    460,
    "preference updates preserve measured geometry",
  );
  assert.equal(demand[0].priority, 200);
  assert.equal(
    pending.size,
    2,
    "each view acquires its own physical observation",
  );
  await borrowedLarge.close();
  assert.equal(
    demand[0].height,
    200,
    "detaching one view preserves its sibling geometry",
  );
  assert.equal(pending.size, 1);
  await borrowedSmall.close();
  assert.equal(
    subscription.isClosed(),
    false,
    "element teardown never closes a borrowed subscription",
  );
  assert.equal(
    demand[0].height,
    100,
    "last detach restores the owner's explicit configuration",
  );
  assert.equal(pending.size, 0);
  Mutation.notify();
  Resize.notify();
  assert.equal(
    demand[0].height,
    100,
    "retired views cannot overwrite the owner's configuration",
  );
  await subscription.close();
  assert.deepEqual(demand, []);
  catalog.close();
  assert.throws(() => handle.subscribe(), { code: "closed" });
});
