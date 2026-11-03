import assert from "node:assert/strict";
import { test } from "node:test";
import { createCaptureSource } from "../../unit-javascript/capture-source.js";
import { LocalMediaRegistry } from "../../unit-javascript/media.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";
import { track } from "../../unit-javascript/track.js";

test("local handle changes source and enablement atomically, without owning capture", () => {
  const native = Object.assign(new EventTarget(), {
    kind: "video",
    readyState: "live",
  });
  const source = createCaptureSource(native, "video");
  const registry = new LocalMediaRegistry(new ResourceOwners(() => {}));
  const handle = registry.track(track.video("camera"));
  const observed = [];
  const unsubscribe = handle.subscribe(() =>
    observed.push([handle.source, handle.enabled]),
  );
  handle.setEnabled(false);
  handle.setSource(source);
  assert.deepEqual(observed.at(-1), [source, false]);
  assert.throws(
    () => handle.setSource(createCaptureSource({ kind: "audio" }, "audio")),
    TypeError,
  );
  assert.equal(handle.source, source);
  handle.setSource(null);
  handle.setEnabled(true);
  assert.deepEqual(observed.at(-1), [null, true]);
  unsubscribe();
  unsubscribe();
  const before = observed.slice();
  registry.close();
  assert.deepEqual(observed, before);
  assert.equal(handle.source, null);
  assert.throws(() => handle.setSource(source), /closed/);
  assert.throws(() => handle.setEnabled(false), /closed/);
  assert.equal(native.readyState, "live");
});
