(() => {
  const peers = [];
  const captures = [];
  const routes = new Map();
  const contexts = [];
  const rejections = [];
  const errors = [];
  addEventListener("error", (event) => errors.push(event.message));
  const NativePeer = RTCPeerConnection;
  window.RTCPeerConnection = class extends NativePeer {
    constructor(...args) {
      super(...args);
      peers.push(this);
    }
  };
  const getUserMedia = navigator.mediaDevices.getUserMedia.bind(
    navigator.mediaDevices,
  );
  navigator.mediaDevices.getUserMedia = async (...args) => {
    const stream = await getUserMedia(...args);
    captures.push(...stream.getTracks());
    return stream;
  };
  const NativeContext = AudioContext;
  window.AudioContext = class extends NativeContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
    }
  };
  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  AudioNode.prototype.connect = function (target, ...args) {
    if (target === this.context.destination && !routes.has(this)) {
      const analyser = this.context.createAnalyser();
      connect.call(this, analyser);
      routes.set(this, analyser);
    }
    return connect.call(this, target, ...args);
  };
  AudioNode.prototype.disconnect = function (...args) {
    routes.delete(this);
    return disconnect.apply(this, args);
  };
  addEventListener("unhandledrejection", (event) =>
    rejections.push(String(event.reason)),
  );
  const wait = async (predicate, label) => {
    for (let i = 0; i < 1500; i++) {
      if (await predicate()) return true;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const videos = [...document.querySelectorAll("video")].map((v) => ({
      width: v.videoWidth,
      frames: v.getVideoPlaybackQuality().totalVideoFrames,
      paused: v.paused,
      readyState: v.readyState,
      error: v.error?.message,
      bounds: v.getBoundingClientRect().toJSON(),
    }));
    const inbound = await Promise.all(
      peers.map(async (peer) =>
        [...(await peer.getStats()).values()].filter(
          (stat) => stat.type === "inbound-rtp",
        ),
      ),
    );
    throw new Error(
      `Quickstart: ${label}; UI: ${document.body.innerText}; peers: ${peers.map((p) => p.connectionState)}; visibility: ${document.visibilityState}; viewport: ${innerWidth}x${innerHeight}; videos: ${JSON.stringify(videos)}; inbound: ${JSON.stringify(inbound)}; rejections: ${rejections}; errors: ${errors}`,
    );
  };
  const video = () => [...document.querySelectorAll("video")];
  const frames = (element) =>
    element.getVideoPlaybackQuality().totalVideoFrames;
  window.__quickstart = {
    wait,
    async ready(identity, remote) {
      await wait(
        () =>
          document.body.innerText.includes(
            `Connection: connected; participant: ${identity}`,
          ),
        "admitted identity",
      );
      await wait(
        () =>
          document.body.innerText.includes(`${remote}: camera`) &&
          document.body.innerText.includes("Remote audio publications: 1"),
        "remote discovery",
      );
      await wait(
        () =>
          video().length === 2 &&
          video().every((v) => v.videoWidth > 0 && frames(v) > 0),
        "local and remote native rendered frames",
      );
      const before = video().map(frames);
      await wait(
        () =>
          video().length === before.length &&
          video().every((v, i) => frames(v) > before[i] + 2),
        "moving video",
      );
      await wait(
        () =>
          routes.size === 1 &&
          [...routes].some(([node, analyser]) => {
            const samples = new Float32Array(analyser.fftSize);
            analyser.getFloatTimeDomainData(samples);
            return (
              node.context.state === "running" &&
              samples.some((value) => Math.abs(value) > 0.00001)
            );
          }),
        "real remote audio output signal",
      );
      if (
        captures.length !== 2 ||
        captures.some((track) => track.readyState !== "live")
      )
        throw new Error("capture ownership");
      if (rejections.length || errors.length)
        throw new Error([...rejections, ...errors].join("; "));
      return true;
    },
    async left() {
      await wait(
        () => document.body.innerText.includes("Connection: disconnected"),
        "disconnect observed",
      );
      await wait(
        () =>
          captures.every((track) => track.readyState === "ended") &&
          routes.size === 0 &&
          peers.every((peer) => peer.connectionState === "closed"),
        "leave releases capture and output",
      );
      return true;
    },
    async unmounted() {
      await wait(
        () =>
          document.querySelector("form") &&
          captures.every((track) => track.readyState === "ended") &&
          peers.every((peer) => peer.connectionState === "closed") &&
          routes.size === 0 &&
          contexts.every((context) => context.state === "closed"),
        "unmount closes Agent and capture",
      );
      if (rejections.length || errors.length)
        throw new Error([...rejections, ...errors].join("; "));
      return true;
    },
  };
  return true;
})();
