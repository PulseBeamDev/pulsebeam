(async () => {
  const { RemoteCatalog } = await import("/dist/remote-catalog.js");
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const RealContext = window.AudioContext;
  const contexts = [];
  window.AudioContext = class extends RealContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
    }
  };
  const source = new RealContext();
  const destination = source.createMediaStreamDestination();
  const oscillator = source.createOscillator();
  oscillator.frequency.value = 660;
  oscillator.connect(destination);
  oscillator.start();
  const track = destination.stream.getAudioTracks()[0];
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
    tracks: { A: { media: track, kind: "audio" } },
  };
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  const blocked = new RemoteCatalog(agent, 0, () => {});
  const before = new RemoteCatalog(agent, 0, () => {});
  const warnings = [];
  const warn = console.warn;
  console.warn = (...args) => warnings.push(args);
  blocked.update(snapshot);
  const voice = blocked.participant("alice").audio("voice");
  const wave = new Float32Array([1, 2]);
  voice.readWaveform(wave);
  assert(
    contexts[0].state === "suspended" &&
      voice.receiving &&
      wave.every((value) => value === 0),
    "blocked playback does not change receiving",
  );
  const button = document.createElement("button");
  button.id = "resume-audio";
  button.textContent = "Resume audio";
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 16;
  const draw = canvas.getContext("2d");
  let color = false;
  const painting = setInterval(() => {
    draw.fillStyle = (color = !color) ? "red" : "blue";
    draw.fillRect(0, 0, 16, 16);
  }, 100);
  const videoTrack = canvas.captureStream(10).getVideoTracks()[0];
  const video = document.createElement("video");
  video.playsInline = true;
  video.style.cssText = "width:160px;height:120px";
  document.body.append(video);
  let retryVideo;
  const attachment = pulsebeam.attachRemoteMedia(
    {
      getSnapshot: () => ({
        tracks: { video: { media: videoTrack } },
      }),
      subscribe: () => () => {},
    },
    video,
    {
      publicationIds: ["video"],
      autoPlay: false,
      onPlaybackBlocked: (_, retry) => {
        retryVideo = retry;
      },
    },
  );
  // The attachment selects video only. A native audio track makes the browser
  // autoplay premise audible, so rejection and retry execute native play().
  video.srcObject.addTrack(track);
  await attachment.retryPlayback();
  const deadline = Date.now() + 5000;
  while (!retryVideo) {
    if (Date.now() > deadline)
      throw new Error("native video was not blocked before trusted gesture");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert(video.paused, "native video autoplay is blocked");
  const state = {
    video,
    videoTrack,
    painting,
    attachment,
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
    ready: false,
    failure: "",
  };
  window.__audioRecovery = state;
  button.onclick = () => {
    // Both resume calls execute directly in this trusted gesture, including the
    // Agent that has no media, lookup, or attachment yet.
    const unlockBefore = before.resumeAudio();
    const unlockBlocked = blocked.resumeAudio();
    void Promise.all([
      unlockBefore,
      unlockBlocked,
      source.resume(),
      retryVideo(),
    ]).then(
      () => {
        state.ready = true;
      },
      (error) => {
        state.failure = String(error);
      },
    );
  };
  document.body.append(button);
  return true;
})();
