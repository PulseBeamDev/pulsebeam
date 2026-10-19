(async () => {
  const { catalog, contexts, RealContext } = window.__earlyAudio;
  const wait = async (condition, label) => {
    for (let i = 0; i < 500; i++) {
      if (condition()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(label);
  };
  const peers = [new RTCPeerConnection(), new RTCPeerConnection()];
  let source;
  let track;
  try {
    await wait(
      () => window.__earlyAudio.ready || window.__earlyAudio.error,
      "early gesture completion",
    );
    if (
      window.__earlyAudio.error ||
      contexts.length !== 1 ||
      contexts[0].state !== "running"
    )
      throw new Error("gesture did not unlock pre-media context");
    source = new RealContext();
    await source.resume();
    const destination = source.createMediaStreamDestination();
    const oscillator = source.createOscillator();
    oscillator.frequency.value = 660;
    oscillator.connect(destination);
    oscillator.start();
    track = destination.stream.getAudioTracks()[0];
    let received;
    peers[1].ontrack = (event) => {
      received = event.track;
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
    await wait(() => received && !received.muted, "late real receiver");
    const context = contexts[0];
    const tap = context.createAnalyser();
    tap.smoothingTimeConstant = 0;
    const sink = context.createGain();
    sink.gain.value = 0;
    tap.connect(sink);
    sink.connect(context.destination);
    const connect = AudioNode.prototype.connect;
    let routes = 0;
    AudioNode.prototype.connect = function (destination, ...args) {
      if (this.context === context && destination === context.destination) {
        routes++;
        connect.call(this, tap);
      }
      return connect.call(this, destination, ...args);
    };
    try {
      catalog.update({
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
      });
    } finally {
      AudioNode.prototype.connect = connect;
    }
    const spectrum = new Float32Array(tap.frequencyBinCount);
    await wait(() => {
      tap.getFloatFrequencyData(spectrum);
      const peak = spectrum.reduce(
        (best, value, index) => (value > spectrum[best] ? index : best),
        0,
      );
      return (
        spectrum[peak] > -40 &&
        Math.abs((peak * context.sampleRate) / tap.fftSize - 660) < 45
      );
    }, "fresh decoded tone through pre-unlocked SDK audio route");
    if (routes !== 1 || contexts.length !== 1 || context.state !== "running")
      throw new Error(
        "late receiver replaced or duplicated pre-unlocked infrastructure",
      );
    return true;
  } finally {
    catalog.close();
    window.AudioContext = RealContext;
    for (const peer of peers) peer.close();
    track?.stop();
    await source?.close();
  }
})();
