import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

// Use the packaged entrypoint through a browser-targeted bundle. Only the
// external media input is modeled; ownership and cursors execute in real WASM.
const Instance = WebAssembly.Instance;
let instances = 0;
WebAssembly.Instance = new Proxy(Instance, {
  construct(target, args, newTarget) {
    instances++;
    return Reflect.construct(target, args, newTarget);
  },
});
const sdk = await import("../../node-sdk.mjs").finally(() => {
  WebAssembly.Instance = Instance;
});
const {
  Agent,
  AgentError,
  AgentStatus,
  ErrorCode,
  PipeStatus,
  VideoSource,
  WatchStatus,
} = sdk;
const config = { endpoint: "http://localhost", token: "node-exact-owners" };
const rejectedWith = (code) => (error) =>
  AgentError.Operation.instanceOf(error) && error.inner.info.code === code;
const secret = "private-test-token";
assert.throws(
  () => new Agent({ endpoint: "invalid-endpoint", token: secret }),
  (error) =>
    rejectedWith(ErrorCode.InvalidArgument)(error) &&
    !String(error).includes(secret) &&
    !error.inner.info.message.includes(secret),
);

const agent = new Agent(config);
const independent = new Agent(config);
assert.equal(
  instances,
  1,
  "independent generated Agents share page-local WASM initialization",
);
assert.ok(agent instanceof Agent);
assert.equal(agent.state().status, AgentStatus.Idle);
assert.notEqual(
  agent.state(),
  independent.state(),
  "snapshots are scoped to their owners",
);
await independent.close();

const initial = agent.state();
assert.ok(Object.isFrozen(initial));
assert.throws(() => agent.renew(""), rejectedWith(ErrorCode.InvalidArgument));
assert.throws(
  () => agent.join("bad\0room"),
  rejectedWith(ErrorCode.InvalidArgument),
);
assert.equal(agent.state(), initial, "invalid intent does not change state");
const first = agent.updates();
const second = agent.updates();
assert.deepEqual(await first.next(), initial);
assert.deepEqual(await second.next(), initial);
const opening = first.next();
await assert.rejects(first.next(), rejectedWith(ErrorCode.Conflict));
agent.renew("replacement-token");
const renewed = agent.state();
assert.ok(
  renewed.desiredCredentialRevision > initial.desiredCredentialRevision,
);
assert.deepEqual(await opening, renewed);
assert.equal(agent.state(), renewed);
const interrupted = first.next();
first.close();
assert.equal(await interrupted, undefined);
assert.deepEqual(await second.next(), renewed);
const additional = Array.from({ length: 31 }, () => agent.updates());
assert.throws(() => agent.updates(), rejectedWith(ErrorCode.CapacityExceeded));
for (const cursor of additional) cursor.close();

const room = agent.room("pinned-room");
const peer = room.participant("peer");
assert.equal(agent.room("pinned-room"), room);
assert.equal(room.participant("peer"), peer);
assert.equal(peer.video("camera"), peer.video("camera"));
assert.notEqual(peer.video("camera"), peer.audio("camera"));
assert.notEqual(peer.video("camera"), agent.video("camera"));
assert.equal(peer.data("events").selector().room, "pinned-room");
assert.equal(
  agent.state().status,
  AgentStatus.Idle,
  "lookup never selects a room",
);

const watch = room.watch();
assert.equal(watch.state().status, WatchStatus.Loading);
assert.equal(
  watch.state().participants,
  undefined,
  "pending is not an empty admitted page",
);
const cursorBytes = Uint8Array.from({ length: 32 }, (_, index) => index);
const continuation = room.watch({ limit: 9, cursor: cursorBytes });
cursorBytes.fill(255);
assert.equal(
  continuation.state().cursor[1],
  1,
  "owner does not alias the caller's cursor",
);
const watchUpdates = watch.updates();
assert.equal((await watchUpdates.next()).status, WatchStatus.Loading);
const watching = watchUpdates.next();
watchUpdates.close();
assert.equal(await watching, undefined);
assert.equal(
  watch.state().status,
  WatchStatus.Loading,
  "closing a cursor leaves its watch open",
);
const watchCapacity = Array.from({ length: 6 }, () => room.watch());
assert.throws(() => room.watch(), rejectedWith(ErrorCode.CapacityExceeded));
await watch.close();
const reuse = room.watch();
assert.equal(watch.state().status, WatchStatus.Closed);
assert.equal(reuse.state().status, WatchStatus.Loading);
await Promise.all([
  reuse.close(),
  continuation.close(),
  ...watchCapacity.map((owner) => owner.close()),
]);

const local = agent.video("camera");
const borrowed = new Track("video");
const capture = VideoSource.fromTrack(borrowed);
const publication = local.publish();
assert.throws(() => local.publish(), rejectedWith(ErrorCode.Conflict));
const pipe = capture.pipeTo(publication);
const publicationUpdates = publication.updates();
const published = publication.state();
assert.ok(Object.isFrozen(published.configuration));
assert.deepEqual(await publicationUpdates.next(), published);
const changing = publicationUpdates.next();
publication.configure({ enabled: false });
assert.equal((await changing).configuration.enabled, false);
assert.equal(
  borrowed.enabled,
  true,
  "muting a publication does not mute capture",
);
const releasing = publicationUpdates.next();
await publication.close();
assert.equal((await releasing).closed, true);
assert.equal(await publicationUpdates.next(), undefined);
assert.equal(pipe.state().status, PipeStatus.Closed);
assert.equal(capture.previewTrack(), borrowed);
const replacement = local.publish();
assert.notEqual(
  replacement,
  publication,
  "a closed label can acquire a fresh publication",
);
assert.equal(replacement.state().configuration.enabled, true);
await replacement.close();
await capture.close();
assert.equal(
  capture.previewTrack(),
  undefined,
  "closed sources cannot produce preview input",
);
assert.equal(
  borrowed.readyState,
  "live",
  "closing the borrowed source never stops capture",
);

const stream = peer.data("events").subscribe();
const pendingData = stream.next();
await assert.rejects(stream.next(), rejectedWith(ErrorCode.Conflict));
const closing = second.next();
await agent.close();
assert.equal(await pendingData, undefined);
assert.equal(stream.state().closed, true);
assert.equal((await closing).status, AgentStatus.Closed);
assert.equal(await second.next(), undefined);
assert.throws(() => agent.join("after-close"), rejectedWith(ErrorCode.Closed));
await agent.close();
