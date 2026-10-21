import assert from "node:assert/strict";
import { test } from "node:test";
import {
  RemoteCatalog,
  attachRemoteVideo,
} from "../../unit-javascript/remote-catalog.js";
import { DataRegistry } from "../../unit-javascript/data.js";
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
          label: "camera",
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
  const owners = new ResourceOwners(() => {});
  owners.activate({
    async resource(command) {
      if (command.op === "claimVideo") return { owner: nextOwner++ };
      if (command.op === "observeVideo") {
        observations.set(command.owner, command.observation);
        demand = [...observations.values()].filter(({ visible }) => visible);
        return { type: "updated" };
      }
      if (command.op === "release") {
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
  const handle = catalog.participant("alice").video("camera");
  handle.setReceiveOptions({ minHeight: 720 });
  const parent = new Video();
  const small = new Video();
  small.parentElement = parent;
  const first = attachRemoteVideo(handle, small);
  await first.ready;
  assert.equal(demand[0].height, 240);
  const large = new Video();
  large.rect.height = 230;
  const second = attachRemoteVideo(handle, large);
  await second.ready;
  assert.equal(demand.length, 2);
  assert.equal(demand[1].height, 460);
  second.close();
  await Promise.resolve();
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
  first.close();
  assert.deepEqual(demand, []);
  assert.equal(small.srcObject, null);
  Resize.notify();
  Mutation.notify();
  Intersection.notify([{ isIntersecting: true }]);
  viewport.dispatchEvent(new Event("resize"));
  assert.deepEqual(demand, [], "teardown cannot resurrect measured demand");
  catalog.close();
  viewport.devicePixelRatio = 2;
});
