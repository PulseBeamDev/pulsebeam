import assert from "node:assert/strict";
import { test } from "node:test";
import { setImmediate } from "node:timers/promises";
import { DataRegistry } from "../../unit-javascript/data.js";
import {
  AudioSource,
  VideoSource,
} from "../../unit-javascript/capture-source.js";
import { LocalMediaRegistry } from "../../unit-javascript/media.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";
import { track } from "../../unit-javascript/track.js";

function nativeTrack(kind = "video") {
  return Object.assign(new EventTarget(), {
    kind,
    readyState: "live",
    stops: 0,
    stop() {
      this.stops++;
      this.readyState = "ended";
    },
  });
}
function host(replace = async () => {}) {
  const commands = [];
  let owner = 0n;
  const runtime = {
    async resource(command) {
      commands.push(command);
      if (command.op === "claimPublication") return { owner: ++owner };
    },
    async replace_owned_track(...args) {
      commands.push({ op: "replace", args });
      if (args[1] && args[1].readyState !== "live")
        throw new Error("capture input has ended");
      await replace(...args);
    },
    async set_owned_muted(...args) {
      commands.push({ op: "mute", args });
    },
  };
  const resources = new ResourceOwners((error) => {
    throw error;
  });
  resources.activate(runtime);
  return { commands, resources, registry: new LocalMediaRegistry(resources) };
}

test("inert local lookup acquires an explicit publication without owning its source", async () => {
  const { commands, resources, registry } = host();
  const native = nativeTrack();
  const source = VideoSource.fromTrack(native);
  const handle = registry.track(track.video("camera"));
  assert.equal(registry.track(track.video("camera")), handle);
  assert.deepEqual(commands, []);
  assert.equal("setSource" in handle, false);
  const publication = await handle.publish(source, { enabled: false });
  assert.equal(commands[0].op, "claimPublication");
  assert.deepEqual(commands[1].args, [
    1n,
    native,
    { contentHint: "motion" },
    true,
    true,
  ]);
  await publication.configure({ enabled: true });
  assert.deepEqual(commands.at(-1), { op: "mute", args: [1n, false] });
  await publication.configureSender({ contentHint: "detail" });
  assert.equal(commands.at(-1).args[4], false);
  await publication.setSource(null);
  assert.equal(commands.at(-1).args[1], null);
  await publication.setSource(source);
  assert.equal(commands.at(-1).args[1], native);
  await assert.rejects(
    publication.setSource(AudioSource.fromTrack(nativeTrack("audio"))),
    TypeError,
  );
  await publication.close();
  await publication.close();
  assert.equal(publication.isClosed(), true);
  assert.equal(source.isClosed(), false);
  assert.equal(native.stops, 0);
  await assert.rejects(publication.configure({ enabled: false }), /closed/);
  await assert.rejects(publication.setSource(source), /closed/);
  registry.close();
  resources.close();
  await source.close();
  await source.close();
  assert.equal(native.stops, 1);
  assert.equal(source.isClosed(), true);
});

test("explicit source close withdraws every borrower even though track.stop emits no ended", async () => {
  const { commands, resources, registry } = host();
  const native = nativeTrack();
  const source = VideoSource.fromTrack(native);
  const a = await registry
    .track(track.video("a"))
    .publish(source, { enabled: true });
  const b = await registry
    .track(track.video("b"))
    .publish(source, { enabled: true });
  await source.close();
  assert.equal(source.isClosed(), true);
  assert.equal(a.isClosed(), false);
  assert.equal(b.isClosed(), false);
  const withdrawals = commands.filter(
    (command) => command.op === "replace" && command.args[1] === null,
  );
  assert.deepEqual(
    withdrawals.map((command) => command.args[0]),
    [1n, 2n],
  );
  await assert.rejects(a.setSource(source), /closed/);
  const replacement = VideoSource.fromTrack(nativeTrack());
  await a.setSource(replacement);
  assert.equal(commands.at(-1).args[1].readyState, "live");
  registry.close();
  resources.close();
  assert.equal(a.isClosed(), true);
  assert.equal(b.isClosed(), true);
  assert.equal(replacement.isClosed(), false);
  await replacement.close();
});

test("superseded queued and in-flight host input failures do not retire the publication", async () => {
  let rejectInput;
  let started;
  const entered = new Promise((resolve) => {
    started = resolve;
  });
  const lateNative = nativeTrack();
  const { commands, resources, registry } = host(async (_owner, track) => {
    if (track === lateNative) {
      started();
      await new Promise((_resolve, reject) => {
        rejectInput = reject;
      });
    }
  });
  const initial = VideoSource.fromTrack(nativeTrack());
  const publication = await registry
    .track(track.video("camera"))
    .publish(initial, { enabled: true });
  const queued = VideoSource.fromTrack(nativeTrack());
  const first = publication.setSource(queued);
  await queued.close();
  await first;
  assert.equal(publication.isClosed(), false);
  assert.equal(commands.at(-1).args[1], null);

  const late = VideoSource.fromTrack(lateNative);
  const second = publication.setSource(late);
  await entered;
  await late.close();
  rejectInput(new Error("capture input has ended"));
  await second;
  assert.equal(publication.isClosed(), false);
  assert.equal(commands.at(-1).args[1], null);
  await publication.setSource(initial);
  await publication.close();
  assert.equal(initial.isClosed(), false);
  await initial.close();
  registry.close();
  resources.close();
});

test("native ended retires a source and terminal Agent retirement cannot revive publication", async () => {
  const { commands, resources, registry } = host();
  const native = nativeTrack("audio");
  const source = AudioSource.fromTrack(native);
  const handle = registry.track(track.audio("microphone"));
  const publication = await handle.publish(source, { enabled: true });
  native.readyState = "ended";
  native.dispatchEvent(new Event("ended"));
  await Promise.resolve();
  assert.equal(source.isClosed(), true);
  assert.equal(commands.at(-1).args[1], null);
  registry.close();
  resources.close();
  await assert.rejects(publication.configure({ enabled: true }), /closed/);
  assert.throws(() => registry.track(track.audio("microphone")), /closed/);
  const before = commands.length;
  resources.activate({
    async resource() {
      throw new Error("must not revive");
    },
  });
  assert.equal(commands.length, before);
});

test("data receiver close awaits old-runtime claims and their late owner release", async () => {
  let finishClaim;
  let finishRelease;
  let startRelease;
  const claim = new Promise((resolve) => {
    finishClaim = resolve;
  });
  const release = new Promise((resolve) => {
    finishRelease = resolve;
  });
  const releasing = new Promise((resolve) => {
    startRelease = resolve;
  });
  const releases = [];
  const registry = new DataRegistry({
    getSnapshot: () => ({ catalog: { tracks: [] } }),
    subscribe: () => () => {},
  });
  registry.activate({
    async resource(command) {
      if (command.op === "receiveData") return await claim;
      assert.equal(command.op, "release");
      releases.push(["old", command.owner]);
      startRelease();
      await release;
    },
  });
  const acquiring = registry.local(track.data("messages", "ordered")).receive();
  registry.activate({
    async resource(command) {
      if (command.op === "receiveData") return { owner: 2n };
      if (command.op === "readData") return null;
      releases.push(["current", command.owner]);
    },
  });
  const receiver = await acquiring;
  const pendingRead = receiver.next();
  let finished = false;
  const closing = receiver.close().then(() => {
    finished = true;
  });
  let registryFinished = false;
  const registryClosing = registry.close().then(() => {
    registryFinished = true;
  });
  assert.deepEqual(await pendingRead, { type: "closed" });
  await setImmediate();
  assert.equal(finished, false);
  assert.equal(registryFinished, false);
  finishClaim({ owner: 1n });
  await releasing;
  assert.equal(finished, false);
  finishRelease();
  await Promise.all([closing, registryClosing]);
  assert.equal(registryFinished, true);
  assert.equal(receiver.isClosed(), true);
  assert.deepEqual(releases, [
    ["current", 2n],
    ["old", 1n],
  ]);
  assert.deepEqual(await receiver.next(), { type: "closed" });
  registry.close();
});
