(async () => {
  const { RemoteCatalog, attachRemoteAudio } = await import(
    "/dist/remote-catalog.js"
  );
  const { DataRegistry } = await import("/dist/data.js");
  const { ResourceOwners } = await import("/dist/resources.js");
  const { BrowserRuntime, whenInitialized } = await import("/dist/wasm.js");
  const RealContext = AudioContext;
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
    catalog: {
      revision: 1,
      participants: [{ participantId: "alice" }],
      publications: [
        { id: "A", participantId: "alice", kind: "audio", label: "voice" },
      ],
    },
    mapping: {
      acceptedIntentRevision: 1,
      video: [],
      audio: [{ receiverIndex: 0, publicationId: "A" }],
    },
    tracks: { A: { media: track, kind: "audio" } },
  };
  snapshot.catalog.tracks = snapshot.catalog.publications;
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  const owners = new ResourceOwners(() => {});
  const catalog = new RemoteCatalog(
    agent,
    new DataRegistry(agent),
    0,
    () => {},
    owners,
  );
  catalog.update(snapshot);
  if (contexts.length !== 0)
    throw new Error("observation created playback without an owner");
  await whenInitialized();
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "late-owned-audio",
    logLevel: "off",
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 0,
      remoteAudios: 2,
    },
  });
  owners.activate(runtime);
  let audioFailure;
  const ownedAudio = attachRemoteAudio(agent, {
    onPlaybackError: (failure) => {
      audioFailure = failure;
    },
  });
  await ownedAudio.ready;
  if (contexts[0].state !== "suspended")
    throw new Error("late-gesture premise was already activated");
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
  document.body.append(video);
  let retry;
  const attachment = pulsebeam.attachRemoteMedia(
    {
      getSnapshot: () => ({ tracks: { video: { media: videoTrack } } }),
      subscribe: () => () => {},
    },
    video,
    {
      publicationIds: ["video"],
      autoPlay: false,
      onPlaybackBlocked: (failure) => {
        retry = failure.retry;
      },
    },
  );
  video.srcObject.addTrack(track);
  await attachment.retryPlayback();
  if (!retry || !video.paused)
    throw new Error(
      "audible attachment was not blocked before trusted gesture",
    );
  const state = {
    catalog,
    runtime,
    source,
    track,
    video,
    videoTrack,
    painting,
    attachment,
    RealContext,
    contexts,
    ready: false,
    error: "",
  };
  window.__audioRecovery = state;
  const button = document.createElement("button");
  button.id = "resume-audio";
  button.textContent = "Resume audio";
  button.onclick = () => {
    void Promise.all([audioFailure.retry(), source.resume(), retry()]).then(
      () => {
        state.ready = true;
      },
      (error) => {
        state.error = String(error);
      },
    );
  };
  document.body.append(button);
  return true;
})();
