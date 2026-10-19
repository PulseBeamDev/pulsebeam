import assert from "node:assert/strict";
import { Track, Stream } from "./platform.mjs";
import {
  lowerMediaStreamTrack,
  liftMediaStreamTrack,
  lowerMediaStream,
  liftMediaStream,
  invalidateMediaForTest,
  exhaustMediaHandlesForTest,
} from "../../node_assets/dist/media-registry.js";

const track = new Track();
const stream = new Stream([track]);
const handle = lowerMediaStreamTrack(track);
assert.equal(
  lowerMediaStreamTrack(track),
  handle,
  "retention preserves identity",
);
assert.equal(liftMediaStreamTrack(handle), track);
const streamHandle = lowerMediaStream(stream);
assert.equal(liftMediaStream(streamHandle), stream);
assert.equal(globalThis.__pulsebeamMediaRegistry.size(), 2n);
assert.throws(() => liftMediaStreamTrack(streamHandle), /stale/);
assert.throws(() => liftMediaStream(handle), /stale/);
assert.throws(() => lowerMediaStreamTrack({}), TypeError);
assert.throws(() => lowerMediaStream({}), TypeError);
assert.equal(invalidateMediaForTest(track), true);
assert.equal(invalidateMediaForTest(track), false);
assert.equal(globalThis.__pulsebeamMediaRegistry.size(), 1n);
assert.throws(() => liftMediaStreamTrack(handle), /stale/);
assert.notEqual(
  lowerMediaStreamTrack(track),
  handle,
  "released identity never reuses stale handle",
);
assert.equal(invalidateMediaForTest(stream), true);
assert.throws(() => liftMediaStream(streamHandle), /stale/);
assert.equal(invalidateMediaForTest(track), true);
assert.equal(globalThis.__pulsebeamMediaRegistry.size(), 0n);
exhaustMediaHandlesForTest();
assert.throws(() => lowerMediaStreamTrack(new Track()), /exhausted/);
assert.throws(() => lowerMediaStream(new Stream()), /exhausted/);
assert.equal(
  track.readyState,
  "live",
  "registry release does not own track lifetime",
);
console.log(
  "emitted media handle identity, type, stale, foreign and exhaustion policy passed",
);
