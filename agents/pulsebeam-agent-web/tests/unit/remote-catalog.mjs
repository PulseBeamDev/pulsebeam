import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteCatalog } from "../../unit-javascript/remote-catalog.js";
import { DataRegistry } from "../../unit-javascript/data.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";

function fixture(capacity = 2) {
  let snapshot = {
    connection: "disconnected",
    participantId: "self",
    catalog: { participants: [], publications: [], tracks: [] },
    mapping: { video: [], audio: [] },
    tracks: {},
  };
  let intent = [];
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  const catalog = new RemoteCatalog(
    agent,
    new DataRegistry(agent),
    capacity,
    (next) => (intent = next),
    new ResourceOwners(() => {}),
  );
  return {
    catalog,
    get intent() {
      return intent;
    },
    update(participants, publications, audio = []) {
      snapshot = {
        ...snapshot,
        catalog: { participants, publications, tracks: publications },
        mapping: { video: [], audio },
      };
      catalog.update(snapshot);
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

test("catalog preserves external-identity handles across absence and publication replacement", () => {
  const f = fixture();
  const aliceHandle = f.catalog.participant("alice");
  const video = aliceHandle.video("camera");
  const audio = aliceHandle.audio("microphone");
  assert.equal(video.active, false);
  assert.equal(audio.receiving, false);
  assert.deepEqual(f.catalog.participants, []);
  f.update([alice], [camera, microphone]);
  const participants = f.catalog.participants;
  const videos = f.catalog.videoTracks;
  const audios = f.catalog.audioTracks;
  assert.equal(participants[0], aliceHandle);
  assert.equal(videos[0], video);
  assert.equal(audios[0], audio);
  assert.equal(video.participantId, "alice");
  assert.notEqual(aliceHandle.audio("camera"), video);
  assert.notEqual(aliceHandle.video("microphone"), audio);
  f.update([alice], [camera, microphone]);
  assert.equal(f.catalog.participants, participants);
  assert.equal(f.catalog.videoTracks, videos);
  assert.equal(f.catalog.audioTracks, audios);
  let notifications = 0;
  video.subscribe(() => notifications++);
  f.update([], []);
  assert.equal(video.active, false);
  assert.equal(notifications, 1);
  f.update([alice], [{ ...camera, id: "new-camera" }]);
  assert.equal(f.catalog.videoTracks[0], video);
  assert.equal(f.catalog.participants[0], aliceHandle);
  assert.equal(video.active, true);
  assert.equal(notifications, 2);
  f.catalog.close();
  assert.equal(video.active, false);
  assert.throws(() => aliceHandle.video("new"), /closed/);
  assert.throws(() => f.catalog.participant("new"), /closed/);
});

test("audio receiving follows mapping, not catalog or media availability, and close clears analysis", () => {
  const f = fixture();
  const audio = f.catalog.participant("alice").audio("microphone");
  let notifications = 0;
  audio.subscribe(() => notifications++);
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
  assert.deepEqual(f.catalog.participants, [f.catalog.participant("alice")]);
  assert.deepEqual(f.catalog.videoTracks, [
    f.catalog.participant("alice").video("camera"),
  ]);
  assert.deepEqual(f.catalog.audioTracks, []);
  f.catalog.close();
});

test("receive validation is atomic, policy alone is inert, and measured demand aggregates without evicting slots", () => {
  const f = fixture(2);
  const publications = Array.from({ length: 3 }, (_, i) => ({
    ...camera,
    id: `id-${i}`,
    label: `camera-${i}`,
  }));
  f.update([alice], publications);
  const [first, second, third] = f.catalog.videoTracks;
  const policy = {
    minHeight: 720,
    minFps: 15,
    priority: 100,
    playoutDelay: { minMs: 3000, maxMs: 500 },
  };
  first.setReceiveOptions(policy);
  policy.playoutDelay.minMs = 0;
  assert.deepEqual(f.intent, []);
  for (const invalid of [
    { minHeight: -1 },
    { minFps: NaN },
    { priority: 2 ** 32 },
    { minHeight: 1.5 },
    { playoutDelay: { minMs: 1, maxMs: Infinity } },
  ])
    assert.throws(() => first.setReceiveOptions(invalid), TypeError);
  const small = Symbol();
  const large = Symbol();
  first.consume(small, 120, true);
  first.consume(large, 460, true);
  second.consume(Symbol(), 100, true);
  const originalWarn = console.warn;
  const warnings = [];
  console.warn = (...args) => warnings.push(args);
  try {
    third.consume(Symbol(), 100, true);
    assert.equal(warnings.length, 1);
    assert.deepEqual(
      f.intent.map(({ slot, trackId }) => ({ slot, trackId })),
      [
        { slot: 0, trackId: "id-0" },
        { slot: 1, trackId: "id-1" },
      ],
    );
    assert.deepEqual(f.intent[0], {
      slot: 0,
      trackId: "id-0",
      height: 460,
      minHeight: 720,
      minFps: 15,
      priority: 100,
      playoutDelay: { mode: "fixed", minMs: 3000, maxMs: 500 },
    });
    first.release(large);
    assert.equal(f.intent[0].height, 120);
    first.consume(small, 120, false);
    assert.deepEqual(
      f.intent.map(({ slot, trackId }) => ({ slot, trackId })),
      [
        { slot: 0, trackId: "id-2" },
        { slot: 1, trackId: "id-1" },
      ],
    );
    f.update([], []);
    assert.deepEqual(f.intent, []);
    f.update([alice], publications);
    assert.equal(f.catalog.videoTracks[0], first);
    assert.ok(f.intent.every(({ trackId }) => trackId !== "id-0"));
    first.consume(small, 120, true);
    assert.ok(f.intent.every(({ trackId }) => trackId !== "id-0"));
    second.close();
    f.catalog.recompute();
    assert.equal(
      f.intent.find(({ trackId }) => trackId === "id-0").minHeight,
      720,
    );
  } finally {
    console.warn = originalWarn;
    f.catalog.close();
  }
});
