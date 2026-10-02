(async () => {
  const { RemoteCatalog } = await import("/dist/remote-catalog.js");
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const state = window.__audioRecovery;
  const {
    blocked,
    before,
    contexts,
    source,
    track,
    voice,
    warnings,
    warn,
    RealContext,
    button,
  } = state;
  const extras = [];
  const peers = [new RTCPeerConnection(), new RTCPeerConnection()];
  const connect = AudioNode.prototype.connect;
  try {
    for (let i = 0; i < 200 && !state.ready && !state.failure; i++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert(
      state.ready &&
        !state.failure &&
        contexts[0].state === "running" &&
        contexts[1].state === "running",
      "trusted gesture recovers blocked and pre-media contexts",
    );
    await blocked.resumeAudio();
    await before.resumeAudio();
    assert(
      voice.receiving && warnings.length > 0,
      "autoplay warning is recoverable and does not alter mapping",
    );
    const resume = contexts[0].resume;
    contexts[0].resume = () => Promise.reject(new Error("resume failed"));
    let failure = false;
    try {
      await blocked.resumeAudio();
    } catch (error) {
      failure = error.message === "resume failed";
    }
    contexts[0].resume = resume;
    assert(
      failure && voice.receiving,
      "explicit recovery rejects failures without changing receiving",
    );
    let received;
    peers[1].ontrack = ({ track }) => {
      received = track;
    };
    peers[0].onicecandidate = ({ candidate }) => {
      if (candidate) void peers[1].addIceCandidate(candidate);
    };
    peers[1].onicecandidate = ({ candidate }) => {
      if (candidate) void peers[0].addIceCandidate(candidate);
    };
    peers[0].addTrack(track, new MediaStream([track]));
    await peers[0].setLocalDescription(await peers[0].createOffer());
    await peers[1].setRemoteDescription(peers[0].localDescription);
    await peers[1].setLocalDescription(await peers[1].createAnswer());
    await peers[0].setRemoteDescription(peers[1].localDescription);
    for (let i = 0; i < 500 && (!received || received.muted); i++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert(received && !received.muted, "receiver arrives after early unlock");
    const snapshot = {
      connection: "connected",
      participantId: "self",
      participantExternalId: "self",
      catalog: {
        revision: 1,
        participants: [{ id: "remote", externalId: "alice" }],
        publications: [
          { id: "A", participantId: "remote", kind: "audio", label: "voice" },
        ],
      },
      mapping: {
        acceptedIntentRevision: 1,
        video: [],
        audio: [{ receiverIndex: 0, publicationId: "A" }],
      },
      tracks: { A: { media: received, kind: "audio" } },
    };
    const unlocked = contexts[1];
    const tap = unlocked.createAnalyser();
    tap.smoothingTimeConstant = 0;
    const sink = unlocked.createGain();
    sink.gain.value = 0;
    connect.call(tap, sink);
    connect.call(sink, unlocked.destination);
    let outputRoutes = 0;
    AudioNode.prototype.connect = function (destination, ...args) {
      if (this.context === unlocked && destination === unlocked.destination) {
        outputRoutes++;
        connect.call(this, tap);
      }
      return connect.call(this, destination, ...args);
    };
    before.update(snapshot);
    AudioNode.prototype.connect = connect;
    const spectrum = new Float32Array(tap.frequencyBinCount);
    const hasDecodedTone = () => {
      tap.getFloatFrequencyData(spectrum);
      const peak = spectrum.reduce(
        (best, value, index) => (value > spectrum[best] ? index : best),
        0,
      );
      const frequency = (peak * unlocked.sampleRate) / tap.fftSize;
      return spectrum[peak] > -40 && Math.abs(frequency - 660) < 45;
    };
    for (let i = 0; i < 500 && !hasDecodedTone(); i++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert(
      contexts.length === 2 &&
        unlocked.state === "running" &&
        outputRoutes === 1 &&
        hasDecodedTone() &&
        document.querySelector("audio") === null,
      "late real receiver plays through pre-unlocked infrastructure without UI",
    );
    const fake = { getSnapshot: () => snapshot, subscribe: () => () => {} };
    const failingContext = new RealContext();
    await failingContext.suspend();
    failingContext.resume = () =>
      Promise.reject(new Error("automatic resume failed"));
    window.AudioContext = class extends RealContext {
      constructor() {
        return failingContext;
      }
    };
    const failed = new RemoteCatalog(fake, 0, () => {});
    extras.push(failed);
    failed.update(snapshot);
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert(
      warnings.some((args) =>
        String(args[1]).includes("automatic resume failed"),
      ),
      "automatic recovery failures use SDK logging",
    );
    let release;
    const delayedContext = new RealContext();
    await delayedContext.suspend();
    delayedContext.resume = () =>
      new Promise((resolve) => {
        release = resolve;
      });
    window.AudioContext = class extends RealContext {
      constructor() {
        return delayedContext;
      }
    };
    const delayed = new RemoteCatalog(fake, 0, () => {});
    extras.push(delayed);
    const pending = delayed.resumeAudio().then(
      () => false,
      () => true,
    );
    delayed.close();
    release();
    assert(await pending, "close fences delayed recovery");
    blocked.close();
    before.close();
    let postClose = false;
    try {
      await before.resumeAudio();
    } catch {
      postClose = true;
    }
    assert(
      postClose &&
        !voice.receiving &&
        contexts.slice(0, 2).every((context) => context.state === "closed"),
      "terminal recovery and resources",
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert(
      window.__pulsebeamUnhandledRejections.length === 0,
      "no unhandled autoplay rejection",
    );
    return true;
  } finally {
    blocked.close();
    before.close();
    extras.forEach((catalog) => catalog.close());
    peers.forEach((peer) => peer.close());
    AudioNode.prototype.connect = connect;
    button.remove();
    track.stop();
    await source.close();
    console.warn = warn;
    window.AudioContext = RealContext;
    delete window.__audioRecovery;
  }
})();
