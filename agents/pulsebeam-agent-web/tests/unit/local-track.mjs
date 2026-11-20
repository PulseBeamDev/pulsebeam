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
import { PipeStatus } from "../../unit-javascript/schema.js";
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
function host(install = async () => {}) {
  const commands = [];
  let owner = 0n;
  const runtime = {
    async resource(command) {},
    resource_now(command) {
      commands.push(command);
      if (command.op === "claimPublication") return { owner: ++owner };
    },
    configure_publication(owner, enabled) {
      commands.push({ op: "configure", owner, enabled });
    },
    publication() {
      return {
        state: () => ({ observationRevision: 0n }),
        updates: () => ({ next: async () => undefined, close() {} }),
      };
    },
    create_owned_publication_pipe(owner) {
      commands.push({ op: "attach", owner });
      let closed = false;
      return {
        state: () => ({
          observationRevision: closed ? 1n : 0n,
          status: closed ? PipeStatus.Closed : PipeStatus.Available,
        }),
        async install_track(source, configuration) {
          commands.push({ op: "install", owner, source, configuration });
          await install(owner, source);
        },
        close() {
          closed = true;
          commands.push({ op: "detach", owner });
          return Promise.resolve();
        },
      };
    },
  };
  const resources = new ResourceOwners((error) => {
    throw error;
  });
  resources.activate(runtime);
  return { commands, resources, registry: new LocalMediaRegistry(resources) };
}

test("publication declaration and input attachment are separate synchronous borrowed lifetimes", async () => {
  const { commands, resources, registry } = host();
  const native = nativeTrack();
  const source = VideoSource.fromTrack(native);
  const handle = registry.track(track.video("camera"));
  assert.equal(registry.track(track.video("camera")), handle);
  assert.deepEqual(commands, []);
  const publication = handle.publish({ enabled: false });
  assert.equal("then" in publication, false);
  assert.equal("setSource" in publication, false);
  assert.deepEqual(
    commands.map(({ op }) => op),
    ["claimPublication", "configure"],
  );
  const pipe = source.pipeTo(publication);
  assert.equal("then" in pipe, false);
  assert.equal(pipe.state(), pipe.state());
  assert.equal(commands.at(-1).source, native);
  publication.configure({ enabled: true });
  assert.deepEqual(commands.at(-1), {
    op: "configure",
    owner: 1n,
    enabled: true,
  });
  await publication.configureSender({ contentHint: "detail" });
  assert.equal(commands.at(-1).configuration.contentHint, "detail");
  assert.throws(
    () => AudioSource.fromTrack(nativeTrack("audio")).pipeTo(publication),
    TypeError,
  );
  await pipe.close();
  assert.equal(pipe.isClosed(), true);
  assert.equal(publication.isClosed(), false);
  assert.equal(source.isClosed(), false);
  const replacement = source.pipeTo(publication);
  await publication.close();
  assert.equal(replacement.isClosed(), true);
  assert.equal(publication.isClosed(), true);
  assert.equal(native.stops, 0);
  assert.throws(() => publication.configure({ enabled: false }), /closed/);
  assert.throws(() => source.pipeTo(publication), /closed/);
  await resources.close();
  registry.close();
  await source.close();
  await source.close();
  assert.equal(native.stops, 1);
});

test("explicit source close withdraws each pipe without closing publications", async () => {
  const { commands, resources, registry } = host();
  const native = nativeTrack();
  const source = VideoSource.fromTrack(native);
  const a = registry.track(track.video("a")).publish();
  const b = registry.track(track.video("b")).publish();
  const first = source.pipeTo(a);
  const second = source.pipeTo(b);
  await source.close();
  assert.equal(first.isClosed(), true);
  assert.equal(second.isClosed(), true);
  assert.equal(a.isClosed(), false);
  assert.equal(b.isClosed(), false);
  assert.deepEqual(
    commands.filter(({ op }) => op === "detach").map(({ owner }) => owner),
    [1n, 2n],
  );
  assert.throws(() => source.pipeTo(a), /closed/);
  const replacement = VideoSource.fromTrack(nativeTrack());
  replacement.pipeTo(a);
  await a.close();
  await b.close();
  await resources.close();
  assert.equal(replacement.isClosed(), false);
  await replacement.close();
});

test("in-flight host input failure after pipe retirement cannot retire the publication", async () => {
  let rejectInput;
  let started;
  const entered = new Promise((resolve) => {
    started = resolve;
  });
  const { resources, registry } = host(async () => {
    started();
    await new Promise((_resolve, reject) => {
      rejectInput = reject;
    });
  });
  const source = VideoSource.fromTrack(nativeTrack());
  const publication = registry.track(track.video("camera")).publish();
  const pipe = source.pipeTo(publication);
  await entered;
  const closing = pipe.close();
  assert.equal(pipe.isClosed(), true);
  assert.equal(publication.isClosed(), false);
  rejectInput(new Error("capture input has ended"));
  await closing;
  assert.equal(source.isClosed(), false);
  await publication.close();
  await resources.close();
  await source.close();
});

test("native ended retires the borrowed pipe, not its publication", async () => {
  const { resources, registry } = host();
  const native = nativeTrack("audio");
  const source = AudioSource.fromTrack(native);
  const publication = registry.track(track.audio("microphone")).publish();
  const pipe = source.pipeTo(publication);
  native.readyState = "ended";
  native.dispatchEvent(new Event("ended"));
  assert.equal(source.isClosed(), true);
  assert.equal(pipe.isClosed(), true);
  assert.equal(publication.isClosed(), false);
  await pipe.close();
  await publication.close();
  registry.close();
  await resources.close();
  assert.throws(() => publication.configure({ enabled: true }), /closed/);
  assert.throws(() => registry.track(track.audio("microphone")), /closed/);
});

test("data subscription close awaits retired and current runtime cleanup", async () => {
  let finishRelease;
  const release = new Promise((resolve) => {
    finishRelease = resolve;
  });
  const releases = [];
  const resources = new ResourceOwners((error) => assert.fail(error));
  const registry = new DataRegistry(
    {
      getSnapshot: () => ({ roomId: "room", catalog: { tracks: [] } }),
      subscribe: () => () => {},
    },
    resources,
  );
  const runtime = (name, owner, cleanup = Promise.resolve()) => ({
    resource_now(command) {
      if (command.op === "release") return;
      assert.equal(command.op, "receiveData");
      assert.deepEqual(command.source, {
        room: "room",
        participantId: "alice",
        trackId: "messages",
      });
      return { owner };
    },
    data_subscription() {
      return {
        state: () => ({ observationRevision: 1n }),
        updates() {
          let resolve;
          return {
            next: () =>
              new Promise((done) => {
                resolve = done;
              }),
            close: () => resolve?.(undefined),
          };
        },
      };
    },
    async resource(command) {
      if (command.op === "readData") return null;
      assert.equal(command.op, "release");
      releases.push([name, command.owner]);
      await cleanup;
    },
  });
  const activate = (value) => {
    resources.activate(value);
  };
  activate(runtime("old", 1n, release));
  const receiver = registry
    .remote(track.data("messages", "ordered"), {
      room: "room",
      participantId: "alice",
    })
    .subscribe();
  assert.equal(receiver instanceof Promise, false);
  activate(runtime("current", 2n));
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
  finishRelease();
  await Promise.all([closing, registryClosing, resources.close()]);
  assert.equal(registryFinished, true);
  assert.equal(receiver.isClosed(), true);
  assert.deepEqual(releases, [
    ["old", 1n],
    ["current", 2n],
  ]);
  assert.deepEqual(await receiver.next(), { type: "closed" });
});
