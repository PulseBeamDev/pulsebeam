(async () => {
  const { RemoteCatalog } = await import("/dist/remote-catalog.js");
  const RealContext = AudioContext;
  const contexts = [];
  window.AudioContext = class extends RealContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
    }
  };
  const catalog = new RemoteCatalog(
    { getSnapshot: () => ({}), subscribe: () => () => {} },
    0,
    () => {},
  );
  const state = { catalog, contexts, RealContext, ready: false, error: "" };
  window.__earlyAudio = state;
  const button = document.createElement("button");
  button.id = "early-audio";
  button.textContent = "Enable playback before media";
  button.onclick = () => {
    void catalog.resumeAudio().then(
      () => {
        state.ready = true;
      },
      (error) => {
        state.error = String(error);
      },
    );
  };
  document.body.append(button);
  if (contexts.length !== 0)
    throw new Error("early premise already created media infrastructure");
  return true;
})();
