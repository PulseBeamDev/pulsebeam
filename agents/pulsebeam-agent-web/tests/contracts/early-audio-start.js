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
  const snapshot = {
    connection: "disconnected",
    catalog: { participants: [], publications: [], tracks: [] },
    mapping: { video: [], audio: [] },
    tracks: {},
  };
  const agent = { getSnapshot: () => snapshot, subscribe: () => () => {} };
  const owners = new ResourceOwners(() => {});
  const catalog = new RemoteCatalog(agent, new DataRegistry(agent), owners);
  catalog.update(snapshot);
  if (contexts.length !== 0)
    throw new Error("observation created media infrastructure");
  await whenInitialized();
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "early-owned-audio",
    logLevel: "off",
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 0,
      remoteAudios: 2,
    },
  });
  owners.activate(runtime);
  const state = {
    catalog,
    contexts,
    RealContext,
    runtime,
    ready: false,
    error: "",
    failure: undefined,
  };
  window.__earlyAudio = state;
  const attachment = attachRemoteAudio(agent, {
    onPlaybackError: (failure) => {
      state.failure = failure;
    },
  });
  await attachment.ready;
  if (
    contexts.length !== 1 ||
    contexts[0].state !== "suspended" ||
    !state.failure
  )
    throw new Error(
      "explicit owner's pre-media autoplay premise was not suspended",
    );
  const button = document.createElement("button");
  button.id = "early-audio";
  button.textContent = "Enable playback before media";
  button.onclick = () => {
    void state.failure.retry().then(
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
