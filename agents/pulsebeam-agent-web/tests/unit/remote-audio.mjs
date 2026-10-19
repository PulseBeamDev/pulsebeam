import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteAudioPlayback } from "../../unit-javascript/remote-audio.js";

test("audio recovery propagates platform errors and fences closed or reset completion", async () => {
  const contexts = [];
  class Context {
    state = "suspended";
    completion = Promise.resolve();
    constructor() {
      contexts.push(this);
    }
    async resume() {
      await this.completion;
      this.state = "running";
    }
    async close() {
      this.state = "closed";
    }
  }
  globalThis.AudioContext = Context;
  const audio = new RemoteAudioPlayback("off");
  await audio.resume();
  const context = contexts[0];
  context.state = "suspended";
  context.completion = Promise.reject(new Error("resume failed"));
  await assert.rejects(audio.resume(), /resume failed/);
  context.completion = Promise.resolve();
  await audio.resume();
  assert.equal(contexts.length, 1, "retry reuses owned context");

  let release;
  context.completion = new Promise((resolve) => {
    release = resolve;
  });
  const reset = audio.resume();
  audio.reset();
  release();
  await assert.rejects(reset, /reset/);
  await audio.resume();
  assert.equal(contexts.length, 2);
  const next = contexts[1];
  next.completion = new Promise((resolve) => {
    release = resolve;
  });
  const pending = audio.resume();
  audio.close();
  release();
  await assert.rejects(pending, /closed/);
  await assert.rejects(audio.resume(), /closed/);
  assert.equal(contexts.length, 2, "closed owner cannot recreate context");
});

test("audio route ownership, pull-read policy, automatic error reporting and replacement use production code", async () => {
  const contexts = [];
  const decoders = [];
  const warnings = [];
  const warn = console.warn;
  console.warn = (...args) => warnings.push(args);
  class Context {
    state = "suspended";
    destination = {};
    sources = new Set();
    analysers = new Set();
    constructor() {
      contexts.push(this);
    }
    async resume() {
      this.state = "running";
    }
    async close() {
      this.state = "closed";
    }
    createMediaStreamSource(stream) {
      const source = {
        stream,
        connect() {},
        disconnect: () => this.sources.delete(source),
      };
      this.sources.add(source);
      return source;
    }
    createAnalyser() {
      const analyser = {
        fftSize: 2048,
        frequencyBinCount: 1024,
        connect() {},
        disconnect: () => this.analysers.delete(analyser),
        getFloatFrequencyData: (buffer) => buffer.fill(-20),
        getFloatTimeDomainData: (buffer) => buffer.fill(0.25),
      };
      this.analysers.add(analyser);
      return analyser;
    }
  }
  globalThis.AudioContext = Context;
  globalThis.document = {
    createElement() {
      const decoder = {
        paused: true,
        srcObject: null,
        async play() {
          this.paused = false;
        },
        pause() {
          this.paused = true;
        },
      };
      decoders.push(decoder);
      return decoder;
    },
  };
  const playback = new RemoteAudioPlayback("warn");
  const first = { kind: "audio", readyState: "live", muted: false };
  const second = { ...first };
  let snapshot = {
    connection: "connected",
    mapping: { audio: [{ receiverIndex: 0, publicationId: "A" }] },
    tracks: { A: { media: first } },
  };
  try {
    playback.update(snapshot);
    await playback.resume();
    const context = contexts[0];
    assert.equal(context.sources.size, 1);
    assert.equal(context.analysers.size, 1);
    const decoder = decoders[0];
    const decoderStream = decoder.srcObject;
    assert.equal(decoder.muted, true);
    assert.equal(decoder.defaultMuted, true);
    assert.equal(decoder.hidden, true);
    assert.deepEqual(decoderStream.getAudioTracks(), [first]);
    const wave = new Float32Array(4096);
    const spectrum = new Float32Array(2048);
    playback.read("A", wave, false);
    playback.read("A", spectrum, true);
    assert.ok(wave.slice(0, 2048).every((value) => value === 0.25));
    assert.ok(wave.slice(2048).every((value) => value === 0));
    assert.ok(spectrum.slice(0, 1024).every((value) => value === -20));
    assert.ok(spectrum.slice(1024).every((value) => value === -Infinity));
    playback.read("A", new Float32Array(), false);
    const short = new Float32Array(8);
    playback.read("A", short, true);
    assert.ok(short.every((value) => value === -20));
    for (let i = 0; i < 100; i++) playback.read("A", wave, false);
    assert.equal(context.sources.size, 1);
    snapshot = {
      ...snapshot,
      mapping: { audio: [{ receiverIndex: 0, publicationId: "B" }] },
      tracks: { B: { media: first } },
    };
    playback.update(snapshot);
    playback.read("A", wave, false);
    assert.ok(
      wave.every((value) => value === 0),
      "retired logical handle clears",
    );
    assert.equal(
      context.sources.size,
      1,
      "same-native remap replaces rather than duplicates route",
    );
    assert.equal(decoders.length, 1);
    assert.equal(decoder.srcObject, decoderStream);
    snapshot = { ...snapshot, tracks: { B: { media: second } } };
    playback.update(snapshot);
    assert.deepEqual(decoderStream.getAudioTracks(), [second]);
    assert.equal(context.sources.size, 1);
    second.muted = true;
    playback.read("B", wave.fill(1), false);
    assert.ok(wave.every((value) => value === 0));
    second.muted = false;
    context.state = "suspended";
    context.resume = () => Promise.reject(new Error("automatic resume failed"));
    playback.update(snapshot);
    for (let turn = 0; turn < 10; turn++) await Promise.resolve();
    assert.ok(
      warnings.some((args) =>
        String(args[1]).includes("automatic resume failed"),
      ),
    );
    context.resume = async () => {
      context.state = "running";
    };
    let releasePlay;
    decoder.play = () =>
      new Promise((resolve) => {
        releasePlay = resolve;
      });
    const pendingPlayback = playback.resume();
    playback.close();
    releasePlay();
    await assert.rejects(pendingPlayback, /closed/);
    assert.equal(context.sources.size, 0);
    assert.equal(context.analysers.size, 0);
    assert.equal(decoder.srcObject, null);
    assert.equal(first.readyState, "live");
    assert.equal(second.readyState, "live");
  } finally {
    playback.close();
    console.warn = warn;
  }
});
