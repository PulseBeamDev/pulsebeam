import assert from "node:assert/strict";
import { Stream, Track } from "../node/platform.mjs";
import {
  exhaustMediaHandlesForTest,
  invalidateMediaForTest,
  liftMediaStream,
  liftMediaStreamTrack,
  lowerInputTrack,
  lowerMediaStream,
  lowerMediaStreamTrack,
  watchTrackLease,
} from "../../unit-javascript/media-registry.js";

const track = new Track();
const stream = new Stream([track]);
const trackId = lowerMediaStreamTrack(track);
const streamId = lowerMediaStream(stream);
assert.notEqual(trackId, streamId);
assert.equal(lowerMediaStreamTrack(track), trackId);
assert.equal(lowerMediaStream(stream), streamId);
assert.equal(liftMediaStreamTrack(trackId), track);
assert.equal(liftMediaStream(streamId), stream);
assert.throws(() => liftMediaStreamTrack(streamId), ReferenceError);
assert.throws(() => liftMediaStream(trackId), ReferenceError);
assert.throws(() => liftMediaStreamTrack(0n), ReferenceError);
assert.throws(() => lowerMediaStreamTrack(stream), TypeError);
assert.equal(invalidateMediaForTest(track), true);
assert.equal(invalidateMediaForTest(track), false);
assert.equal(track.readyState, "live");
assert.throws(() => liftMediaStreamTrack(trackId), ReferenceError);
assert.notEqual(
  lowerMediaStreamTrack(track),
  trackId,
  "released ids are not reused",
);

const registry = globalThis.__pulsebeamMediaRegistry;
const first = registry.retainTrackLease(track);
const second = registry.retainTrackLease(track);
assert.notEqual(
  first,
  second,
  "independent retained tracks have independent leases",
);
let changed = 0;
const stop = watchTrackLease(track, () => {
  changed++;
});
assert.ok(registry.release(first, "track"));
assert.equal(changed, 1);
assert.equal(liftMediaStreamTrack(second), track);
track.stop();
assert.ok(registry.release(second, "track"));
assert.equal(changed, 2);
stop();

const input = lowerInputTrack(track);
assert.equal(registry.takeInput(input), track);
assert.equal(registry.takeInput(input), undefined);
const abandoned = lowerInputTrack(track);
await Promise.resolve();
assert.equal(registry.takeInput(abandoned), undefined);
exhaustMediaHandlesForTest();
assert.throws(() => lowerMediaStream(new Stream()), RangeError);
assert.throws(() => lowerInputTrack(track), RangeError);
