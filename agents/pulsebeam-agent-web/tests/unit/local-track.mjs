import assert from "node:assert/strict";
import { test } from "node:test";
import {
  LocalTrackHandle,
  LocalTrackCapacityError,
} from "../../unit-javascript/local-track.js";

test("local handle applies source and enablement atomically, and release is terminal", () => {
  const source = { kind: "video" };
  let applied, enabled;
  const handle = new LocalTrackHandle(
    "video",
    "camera",
    (next) => {
      if (next?.invalid) throw new TypeError("invalid capture");
      applied = next;
    },
    (next) => {
      enabled = next;
    },
  );
  const observed = [];
  const unsubscribe = handle.subscribe(() =>
    observed.push([handle.source, handle.enabled]),
  );
  handle.setEnabled(false);
  handle.setSource(source);
  assert.equal(applied, source);
  assert.equal(enabled, false);
  assert.deepEqual(observed.at(-1), [source, false]);
  assert.throws(() => handle.setSource({ invalid: true }), TypeError);
  assert.equal(handle.source, source);
  handle.setSource(null);
  handle.setEnabled(true);
  assert.deepEqual(observed.at(-1), [null, true]);
  unsubscribe();
  unsubscribe();
  const before = observed.slice();
  handle.clear();
  assert.deepEqual(observed, before);
  assert.equal(handle.source, null);
  assert.throws(() => handle.setSource(source), /closed/);
  assert.throws(() => handle.setEnabled(false), /closed/);
  assert.equal(source.kind, "video");
});

test("capacity errors preserve public diagnostic fields", () => {
  const error = new LocalTrackCapacityError("video", "screen", 1);
  assert.ok(error instanceof RangeError);
  assert.deepEqual(
    [error.name, error.kind, error.label, error.capacity],
    ["LocalTrackCapacityError", "video", "screen", 1],
  );
});
