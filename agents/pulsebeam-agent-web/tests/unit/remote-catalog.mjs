import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteCatalog } from "../../unit-javascript/remote-catalog.js";
import { DataRegistry } from "../../unit-javascript/data.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";
import {
  subscribeVideo,
  selectAudio,
} from "../../unit-javascript/reception.js";

function fixture() {
  const listeners = new Set();
  let snapshot = {
    connection: "disconnected",
    roomId: "room",
    participantId: "self",
    catalog: { participants: [], publications: [], tracks: [] },
    mapping: { video: [], audio: [] },
    tracks: {},
  };
  const agent = {
    getSnapshot: () => snapshot,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  const catalog = new RemoteCatalog(
    agent,
    new DataRegistry(agent),
    new ResourceOwners(() => {}),
  );
  return {
    catalog,
    update(participants, publications, audio = [], roomId = "room") {
      snapshot = {
        ...snapshot,
        roomId,
        catalog: { participants, publications, tracks: publications },
        mapping: { video: [], audio },
      };
      catalog.update(snapshot);
      for (const listener of listeners) listener();
    },
  };
}
const alice = { participantId: "alice" };
const camera = {
  id: "camera-id",
  participantId: alice.participantId,
  kind: "video",
  label: "camera",
};
const microphone = {
  id: "microphone-id",
  participantId: alice.participantId,
  kind: "audio",
  label: "microphone",
};

test("catalog keeps discovery identity stable across publication replacement", () => {
  const f = fixture();
  const aliceHandle = f.catalog.room("room").participant("alice");
  const video = aliceHandle.video("camera");
  const audio = aliceHandle.audio("microphone");
  const publicParticipant = f.catalog.room("room").participant("alice");
  assert.equal(publicParticipant.id(), "alice");
  assert.equal(
    publicParticipant.video("camera"),
    publicParticipant.video("camera"),
  );
  assert.notEqual(
    publicParticipant.audio("camera"),
    publicParticipant.video("camera"),
  );
  assert.equal("track" in publicParticipant, false);
  assert.equal(video.active, false);
  assert.equal(audio.receiving, false);
  assert.deepEqual(f.catalog.participants, []);
  f.update([alice], [camera, microphone]);
  const participants = f.catalog.participants;
  const videos = f.catalog.videoTracks;
  const audios = f.catalog.audioTracks;
  assert.notEqual(participants[0], aliceHandle);
  assert.notEqual(videos[0], video);
  assert.notEqual(audios[0], audio);
  assert.equal(video.id, camera.id);
  assert.equal(audio.id, microphone.id);
  assert.equal(aliceHandle.active, true);
  assert.equal(aliceHandle.tracks.length, 2);
  assert.equal(video.participantId, "alice");
  assert.notEqual(aliceHandle.audio("camera"), video);
  assert.notEqual(aliceHandle.video("microphone"), audio);
  f.update([alice], [camera, microphone]);
  assert.equal(f.catalog.participants, participants);
  assert.equal(f.catalog.videoTracks, videos);
  assert.equal(f.catalog.audioTracks, audios);
  let notifications = 0;
  video.watch(() => notifications++);
  f.update([], []);
  assert.equal(video.active, false);
  assert.equal(aliceHandle.active, false);
  assert.deepEqual(aliceHandle.tracks, []);
  assert.equal(notifications, 1);
  f.update([alice], [{ ...camera, id: "new-camera" }]);
  assert.equal(f.catalog.videoTracks[0], videos[0]);
  assert.notEqual(f.catalog.participants[0], aliceHandle);
  assert.equal(video.active, true);
  assert.equal(notifications, 2);
  f.catalog.close();
  assert.equal(video.active, false);
  assert.throws(() => aliceHandle.video("new"), /closed/);
  assert.throws(() => f.catalog.room("room").participant("new"), /closed/);
});

test("audio mapping tracks discovery and clears on close", () => {
  const f = fixture();
  const audio = f.catalog.room("room").participant("alice").audio("microphone");
  let notifications = 0;
  audio.watch(() => notifications++);
  f.update([alice], [microphone]);
  assert.equal(audio.receiving, false);
  f.update(
    [alice],
    [microphone],
    [{ publicationId: microphone.id, receiverIndex: 0 }],
  );
  assert.equal(audio.receiving, true);
  const mappedNotifications = notifications;
  f.update(
    [alice],
    [microphone],
    [{ publicationId: microphone.id, receiverIndex: 0 }],
  );
  assert.equal(notifications, mappedNotifications);
  f.update([alice], [microphone]);
  assert.equal(audio.receiving, false);
  assert.equal(notifications, mappedNotifications + 1);
  f.catalog.close();
  const wave = new Float32Array(9).fill(1);
  const spectrum = new Float32Array(9).fill(1);
  audio.readWaveform(wave);
  audio.readSpectrum(spectrum);
  assert.ok(wave.every((value) => value === 0));
  assert.ok(spectrum.every((value) => value === -Infinity));
});

test("self is excluded by admitted application participant identity", () => {
  const f = fixture();
  f.update(
    [{ participantId: "self" }, alice],
    [
      { ...camera, id: "self-camera", participantId: "self" },
      { ...microphone, participantId: "self" },
      camera,
    ],
  );
  assert.equal(f.catalog.participants.length, 1);
  assert.equal(f.catalog.participants[0].participantId, "alice");
  assert.equal(f.catalog.videoTracks.length, 1);
  assert.equal(f.catalog.videoTracks[0].id, camera.id);
  assert.deepEqual(f.catalog.audioTracks, []);
  f.catalog.close();
});

test("pinned references hide discovery metadata after a room move", () => {
  const f = fixture();
  const participant = f.catalog.room("room").participant("alice");
  const pinned = participant.video("camera");
  f.update([alice], [camera]);
  assert.equal(pinned.id, camera.id);
  f.update([alice], [{ ...camera, id: "other-room-camera" }], [], "other-room");
  assert.equal(pinned.id, null);
  assert.equal(pinned.active, false);
  assert.deepEqual(participant.tracks, []);
  assert.equal(f.catalog.videoTracks[0].id, "other-room-camera");
  f.catalog.close();
});

test("receive option validation is atomic and inert", () => {
  const f = fixture();
  f.update([alice], [camera]);
  const video = f.catalog.videoTracks[0];
  const policy = {
    minHeight: 720,
    minFps: 15,
    priority: 100,
    playoutDelay: { minMs: 5, maxMs: 30 },
  };
  video.setReceiveOptions(policy);
  policy.playoutDelay.minMs = 0;
  assert.deepEqual(video.options, {
    minHeight: 720,
    minFps: 15,
    priority: 100,
    playoutDelay: { minMs: 5, maxMs: 30 },
  });
  for (const invalid of [
    { minHeight: -1 },
    { minFps: NaN },
    { priority: 2 ** 32 },
    { minHeight: 1.5 },
    { playoutDelay: { minMs: 1, maxMs: Infinity } },
  ])
    assert.throws(() => video.setReceiveOptions(invalid), TypeError);
  assert.throws(
    () => video.setReceiveOptions({ playoutDelay: { minMs: 2, maxMs: 1 } }),
    RangeError,
  );
  f.catalog.close();
});

test("subscription close drains a late physical observation without adopting it", async () => {
  const commands = [];
  let finish;
  const pending = new Promise((resolve) => {
    finish = resolve;
  });
  const resources = new ResourceOwners((error) => {
    throw error;
  });
  resources.activate({
    async resource(command) {
      if (command.op !== "release") return this.resource_now(command);
    },
    video_subscription: () => ({
      state() {},
      updates: () => ({ async next() {}, close() {} }),
    }),
    resource_now(command) {
      commands.push(command);
      return command.op === "claimVideo" ? { owner: 9n } : undefined;
    },
    observe_media(owner, source) {
      assert.equal(owner, 9n);
      assert.equal(source, undefined);
      return pending;
    },
  });
  const selector = { room: "room", participantId: "alice", label: "camera" };
  const configuration = {
    visible: true,
    height: 720,
    minHeight: 0,
    minFps: 0,
    priority: 0,
    playoutDelay: { mode: "adaptive" },
  };
  const subscription = subscribeVideo(resources, selector, configuration);
  assert.equal("then" in subscription, false);
  assert.deepEqual(commands.slice(0, 2), [
    { op: "claimVideo", selector },
    { op: "observeVideo", owner: 9n, observation: configuration },
  ]);
  let freed = 0;
  let stopped = false;
  const acquiring = assert.rejects(subscription.observe(), { code: "closed" });
  const closing = subscription.close();
  assert.equal(subscription.isClosed(), true);
  finish({
    track: () => undefined,
    is_closed: () => stopped,
    async close() {
      stopped = true;
    },
    free() {
      freed++;
    },
  });
  await Promise.all([acquiring, closing]);
  assert.equal(stopped, true);
  assert.equal(freed, 1);
  assert.equal(
    commands.filter((command) => command.op === "release").length,
    1,
  );
  assert.equal(subscription.close(), closing);
  await resources.close();
});

test("invalid initial subscription configuration retires its claim in the same turn", async () => {
  const commands = [];
  const failure = {
    pulsebeamInfo: { code: "InvalidArgument", message: "Invalid observation" },
  };
  const resources = new ResourceOwners(() => assert.fail("unexpected report"));
  resources.activate({
    resource_now(command) {
      commands.push(command.op);
      if (command.op === "claimVideo") return { owner: 9n };
      if (command.op === "observeVideo") throw failure;
    },
    async resource(command) {
      assert.equal(command.op, "release");
    },
  });
  assert.throws(
    () =>
      subscribeVideo(
        resources,
        { room: "room", participantId: "alice", label: "camera" },
        {},
      ),
    (error) => {
      assert.deepEqual(error.info, failure.pulsebeamInfo);
      assert.equal(Object.isFrozen(error.info), true);
      return true;
    },
  );
  assert.deepEqual(commands, ["claimVideo", "observeVideo", "release"]);
  await resources.close();
});

test("audio selection reattachment claims the latest explicit policy", async () => {
  const resources = new ResourceOwners((error) => {
    throw error;
  });
  resources.activate({
    async resource(command) {
      return this.resource_now(command);
    },
    resource_now(command) {
      return command.op === "claimAudioPolicy" ? { owner: 1n } : undefined;
    },
  });
  const initial = {
    automatic: false,
    pinned: [],
    selected: [],
    playoutDelays: [],
  };
  const selection = await selectAudio(resources, initial);
  const next = {
    ...initial,
    selected: [{ room: "room", participantId: "alice", label: "microphone" }],
  };
  await selection.configure(next);
  let configured;
  const applied = new Promise((resolve) => {
    configured = resolve;
  });
  const commands = [];
  resources.activate({
    async resource(command) {
      if (command.op !== "release") return this.resource_now(command);
    },
    resource_now(command) {
      commands.push(command);
      if (command.op === "claimAudioPolicy") return { owner: 2n };
      if (command.op === "configureAudioPolicy") configured();
    },
  });
  await applied;
  assert.deepEqual(commands[0], { op: "claimAudioPolicy", policy: next });
  assert.deepEqual(commands[1], {
    op: "configureAudioPolicy",
    owner: 2n,
    policy: next,
  });
  await selection.close();
  await resources.close();
});
