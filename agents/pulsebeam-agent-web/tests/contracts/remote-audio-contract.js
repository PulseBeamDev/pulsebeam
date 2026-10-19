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
    const voice = catalog.participant("alice").audio("first");
    const spectrum = new Float32Array(1024);
    voice.readSpectrum(spectrum);
    assert(
      Math.abs((peak(spectrum) * sdkContext.sampleRate) / 2048 - 440) < 45,
      "SDK analysis exposes the real decoded tone",
    );
    snapshot = {
      ...snapshot,
      tracks: { A: { media: received[1], kind: "audio" } },
    };
    catalog.update(snapshot);
    await wait(() => {
      voice.readSpectrum(spectrum);
      return (
        Math.abs((peak(spectrum) * sdkContext.sampleRate) / 2048 - 1320) < 45
      );
    }, "replacement receiver reaches SDK analyser");
    await wait(
      () => Math.abs(outputFrequency() - 1320) < 45,
      "replacement receiver reaches SDK output",
    );
    assert(
      output.routes.size === 1 &&
        decoders.length === 1 &&
        decoder.srcObject === decoderStream &&
        decoderStream.getAudioTracks().length === 1 &&
        decoderStream.getAudioTracks()[0] === received[1],
      "one native decoder and output route follow the fresh receiver",
    );
    catalog.close();
    await wait(
      () => sdkContext.state === "closed",
      "closed native audio context",
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
