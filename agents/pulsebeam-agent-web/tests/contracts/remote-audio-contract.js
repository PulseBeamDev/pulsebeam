(async () => {
  const { RemoteCatalog } = await import("/dist/remote-catalog.js");
  const RealContext = window.AudioContext;
  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  const contexts = [];
  const outputs = new Map();
  const decoders = [];
  const createElement = document.createElement;
  document.createElement = function (name, ...args) {
    const element = createElement.call(this, name, ...args);
    if (name === "audio") decoders.push(element);
    return element;
  };
  // Independently tee actual SDK destination connections through a silent probe.
  window.AudioContext = class extends RealContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
      const tap = this.createAnalyser();
      tap.smoothingTimeConstant = 0;
      const sink = this.createGain();
      sink.gain.value = 0;
      connect.call(tap, sink);
      connect.call(sink, this.destination);
      outputs.set(this, { tap, routes: new Set() });
    }
  };
  AudioNode.prototype.connect = function (destination, ...args) {
    const probe = outputs.get(this.context);
    if (destination === this.context.destination) {
      probe.routes.add(this);
      connect.call(this, probe.tap);
    }
    return connect.call(this, destination, ...args);
  };
  AudioNode.prototype.disconnect = function (...args) {
    outputs.get(this.context)?.routes.delete(this);
    return disconnect.apply(this, args);
  };
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const wait = async (predicate, label, diagnostics = () => "") => {
    for (let i = 0; i < 500; i++) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(`audio condition: ${label} ${await diagnostics()}`);
  };
  const sourceContext = new RealContext();
  const left = new RTCPeerConnection();
  const right = new RTCPeerConnection();
  const catalogs = [];
  const captured = [];
  try {
    await sourceContext.resume();
    const synth = (frequency) => {
      const oscillator = sourceContext.createOscillator();
      oscillator.frequency.value = frequency;
      const gain = sourceContext.createGain();
      gain.gain.value = 0.2;
      const destination = sourceContext.createMediaStreamDestination();
      oscillator.connect(gain).connect(destination);
      oscillator.start();
      const track = destination.stream.getAudioTracks()[0];
      captured.push(track);
      left.addTrack(track, destination.stream);
    };
    synth(440);
    synth(1320);
    const received = [];
    right.ontrack = ({ track }) => received.push(track);
    left.onicecandidate = ({ candidate }) => {
      if (candidate) void right.addIceCandidate(candidate);
    };
    right.onicecandidate = ({ candidate }) => {
      if (candidate) void left.addIceCandidate(candidate);
    };
    await left.setLocalDescription(await left.createOffer());
    await right.setRemoteDescription(left.localDescription);
    await right.setLocalDescription(await right.createAnswer());
    await left.setRemoteDescription(right.localDescription);
    await wait(
      () => received.length === 2 && received.every((track) => !track.muted),
      "real decoded receivers",
    );
    const publication = (id, label) => ({
      id,
      label,
      kind: "audio",
      participantId: "remote",
    });
    let snapshot = {
      connection: "connected",
      participantId: "self",
      participantExternalId: "self",
      catalog: {
        revision: 1,
        participants: [{ id: "remote", externalId: "alice" }],
        publications: [publication("A", "first"), publication("B", "second")],
      },
      mapping: {
        acceptedIntentRevision: 1,
        video: [],
        audio: [{ receiverIndex: 0, publicationId: "A" }],
      },
      tracks: { A: { media: received[0], kind: "audio" } },
    };
    const fake = { getSnapshot: () => snapshot, subscribe: () => () => {} };
    const catalog = new RemoteCatalog(fake, 0, () => {});
    catalogs.push(catalog);
    // No lookup, pull read or UI mount starts playback.
    catalog.update(snapshot);
    const sdkContext = contexts[0];
    const output = outputs.get(sdkContext);
    const peak = (data) =>
      data.reduce(
        (best, value, index) => (value > data[best] ? index : best),
        0,
      );
    const outputFrequency = () => {
      const data = new Float32Array(output.tap.frequencyBinCount);
      output.tap.getFloatFrequencyData(data);
      return (peak(data) * sdkContext.sampleRate) / output.tap.fftSize;
    };
    await wait(
      () => Math.abs(outputFrequency() - 440) < 45,
      "SDK output without UI",
      async () =>
        JSON.stringify({
          context: sdkContext.state,
          source: sourceContext.state,
          frequency: outputFrequency(),
          routes: output.routes.size,
          receivers: [...(await right.getStats()).values()].filter(
            (entry) => entry.type === "inbound-rtp",
          ),
        }),
    );
    assert(
      output.routes.size === 1 && document.querySelector("audio") === null,
      "exactly one audible route without application audio element",
    );
    const decoder = decoders[0];
    const decoderStream = decoder.srcObject;
    assert(
      decoders.length === 1 &&
        decoder.muted &&
        decoder.defaultMuted &&
        decoder.hidden &&
        !decoder.controls &&
        !decoder.isConnected &&
        decoderStream.getAudioTracks().length === 1 &&
        decoderStream.getAudioTracks()[0] === received[0],
      "one private detached hidden muted receiver decoder",
    );
    const participant = catalog.participant("alice");
    const a = participant.audio("first");
    const b = participant.audio("second");
    let notifications = 0;
    a.subscribe(() => notifications++);
    b.subscribe(() => notifications++);
    const wave = new Float32Array(4096);
    const spectrum = new Float32Array(2048);
    a.readWaveform(wave);
    a.readSpectrum(spectrum);
    assert(
      a.receiving &&
        !b.receiving &&
        wave.some((value) => Math.abs(value) > 0.05) &&
        wave.every((value) => Math.abs(value) <= 1),
      "normalized decoded waveform",
    );
    assert(
      wave.slice(2048).every((value) => value === 0) &&
        spectrum.slice(1024).every((value) => value === -Infinity),
      "oversized silence tails",
    );
    assert(
      Math.abs((peak(spectrum) * sdkContext.sampleRate) / 2048 - 440) < 45 &&
        Math.max(...spectrum) < 0,
      "ascending decibel spectrum",
    );
    const short = new Float32Array(8);
    a.readSpectrum(short);
    assert(
      short.every((value, index) => value === spectrum[index]),
      "short buffer does not change FFT",
    );
    a.readWaveform(new Float32Array());
    a.readSpectrum(new Float32Array());
    for (let i = 0; i < 100; i++) a.readWaveform(wave);
    assert(
      notifications === 0 && output.routes.size === 1,
      "pull reads do not notify or duplicate output",
    );
    // Direct remapping with exactly the same native receiver track.
    snapshot = {
      ...snapshot,
      mapping: {
        ...snapshot.mapping,
        audio: [{ receiverIndex: 0, publicationId: "B" }],
      },
      tracks: { B: { media: received[0], kind: "audio" } },
    };
    catalog.update(snapshot);
    a.readWaveform(wave.fill(3));
    a.readSpectrum(spectrum.fill(3));
    assert(
      !a.receiving &&
        b.receiving &&
        notifications === 2 &&
        wave.every((value) => value === 0) &&
        spectrum.every((value) => value === -Infinity),
      "old logical handle clears on same-native reassignment",
    );
    assert(
      output.routes.size === 1,
      "reassignment replaces route instead of duplicating it",
    );
    assert(
      decoders.length === 1 && decoder.srcObject === decoderStream,
      "same-native logical reassignment does not recreate the decoder",
    );
    // Receiver recreation switches decoded tones and cannot retain analyser history.
    snapshot = {
      ...snapshot,
      tracks: { B: { media: received[1], kind: "audio" } },
    };
    catalog.update(snapshot);
    b.readSpectrum(spectrum);
    assert(
      !spectrum.some((value) => value > -30),
      "new analyser has no old tone history",
    );
    await wait(() => {
      b.readSpectrum(spectrum);
      return (
        Math.abs((peak(spectrum) * sdkContext.sampleRate) / 2048 - 1320) < 45
      );
    }, "replacement receiver signal follows B");
    // The persistent destination probe has its own render/FFT history.
    await wait(
      () => Math.abs(outputFrequency() - 1320) < 45,
      "replacement receiver reaches SDK output",
      () =>
        JSON.stringify({
          frequency: outputFrequency(),
          routes: output.routes.size,
          context: sdkContext.state,
        }),
    );
    assert(
      Math.abs(outputFrequency() - 1320) < 45 && output.routes.size === 1,
      "destination follows replacement",
    );
    assert(
      decoder.srcObject.getAudioTracks().length === 1 &&
        decoder.srcObject.getAudioTracks()[0] === received[1] &&
        received[0].readyState === "live",
      "decoder follows receiver recreation without stopping borrowed tracks",
    );
    a.readSpectrum(spectrum.fill(9));
    assert(
      spectrum.every((value) => value === -Infinity),
      "A never observes B's tone",
    );
    const independent = new RemoteCatalog(fake, 0, () => {});
    catalogs.push(independent);
    independent.update(snapshot);
    const otherContext = contexts[1];
    const otherOutput = outputs.get(otherContext);
    const otherFrequency = () => {
      const data = new Float32Array(otherOutput.tap.frequencyBinCount);
      otherOutput.tap.getFloatFrequencyData(data);
      return (peak(data) * otherContext.sampleRate) / otherOutput.tap.fftSize;
    };
    await wait(
      () => Math.abs(otherFrequency() - 1320) < 45,
      "independent output",
    );
    assert(
      decoders.length === 2 &&
        decoders[1] !== decoder &&
        decoders[1].muted &&
        decoders[1].hidden &&
        !decoders[1].isConnected,
      "independent Agent owns its own private decoder",
    );
    assert(
      independent.participant("alice").audio("second") !== b,
      "Agent isolation",
    );
    snapshot = {
      ...snapshot,
      connection: "disconnected",
      mapping: { ...snapshot.mapping, audio: [] },
      tracks: {},
    };
    catalog.update(snapshot);
    b.readWaveform(wave.fill(4));
    assert(
      !b.receiving &&
        wave.every((value) => value === 0) &&
        output.routes.size === 0,
      "disconnect removes audible media and analysis",
    );
    assert(
      decoder.paused && decoder.srcObject === null,
      "disconnect clears and pauses private decoding",
    );
    snapshot = {
      ...snapshot,
      connection: "connected",
      mapping: {
        ...snapshot.mapping,
        audio: [{ receiverIndex: 0, publicationId: "B" }],
      },
      tracks: { B: { media: received[1], kind: "audio" } },
    };
    catalog.update(snapshot);
    await wait(
      () => Math.abs(outputFrequency() - 1320) < 45,
      "reconnected output",
    );
    assert(
      b.receiving && output.routes.size === 1 && contexts.length === 2,
      "reconnect reuses infrastructure without duplicate routes",
    );
    assert(
      decoders.length === 2 && decoder.srcObject !== null && !decoder.paused,
      "reconnect reuses the private decoder",
    );
    const nativePlay = decoder.play;
    let releasePlay;
    decoder.play = () =>
      new Promise((resolve) => {
        releasePlay = resolve;
      });
    const pendingRecovery = catalog.resumeAudio().then(
      () => false,
      () => true,
    );
    catalog.close();
    catalog.close();
    releasePlay();
    assert(await pendingRecovery, "close fences pending decoder play recovery");
    decoder.play = nativePlay;
    await wait(() => sdkContext.state === "closed", "closed audio context");
    assert(
      outputs.get(otherContext).routes.size === 1 &&
        otherContext.state === "running" &&
        Math.abs(otherFrequency() - 1320) < 45 &&
        !decoders[1].paused,
      "closing one Agent leaves another audible",
    );
    assert(
      decoder.paused && decoder.srcObject === null && !decoder.isConnected,
      "close releases private decoder media",
    );
    b.readSpectrum(spectrum.fill(4));
    assert(
      !b.receiving && spectrum.every((value) => value === -Infinity),
      "terminal reads clear",
    );
    let rejected = false;
    try {
      await catalog.resumeAudio();
    } catch {
      rejected = true;
    }
    assert(
      rejected &&
        captured.every((track) => track.readyState === "live") &&
        received.every((track) => track.readyState === "live"),
      "close rejects recovery and leaves borrowed media live",
    );
    return true;
  } finally {
    catalogs.forEach((catalog) => catalog.close());
    left.close();
    right.close();
    captured.forEach((track) => track.stop());
    await sourceContext.close();
    document.createElement = createElement;
    window.AudioContext = RealContext;
    AudioNode.prototype.connect = connect;
    AudioNode.prototype.disconnect = disconnect;
  }
})();
