(() => {
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const wait = async (predicate, label) => {
    for (let i = 0; i < 1500; i++) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const transports = await Promise.all(
      [...peers].map(async (peer) => ({
        connection: peer.connectionState,
        ice: peer.iceConnectionState,
        gathering: peer.iceGatheringState,
        signaling: peer.signalingState,
        stats: [...(await peer.getStats()).values()].filter((stat) =>
          [
            "transport",
            "candidate-pair",
            "local-candidate",
            "remote-candidate",
          ].includes(stat.type),
        ),
      })),
    );
    throw new Error(
      `Meet media condition: ${label}; UI: ${document.body.innerText.slice(0, 1500)}; transports: ${JSON.stringify(transports)}`,
    );
  };
  const peers = new Set();
  const addTransceiver = RTCPeerConnection.prototype.addTransceiver;
  RTCPeerConnection.prototype.addTransceiver = function (...args) {
    peers.add(this);
    return addTransceiver.apply(this, args);
  };
  const contexts = [];
  let audioGesture = false;
  let blockedResumes = 0;
  for (const type of ["pointerdown", "keydown"]) {
    addEventListener(
      type,
      (event) => {
        if (event.isTrusted) audioGesture = true;
      },
      true,
    );
  }
  const RealContext = AudioContext;
  window.AudioContext = class extends RealContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
      // Chrome exempts active capture from autoplay restrictions. Exercise the
      // blocked-policy branch while retaining native decoding and output.
      void this.suspend();
    }
    resume() {
      if (!audioGesture) {
        blockedResumes++;
        return Promise.reject(
          new DOMException("gesture required", "NotAllowedError"),
        );
      }
      return super.resume();
    }
  };
  const sources = new Map();
  const createSource = RealContext.prototype.createMediaStreamSource;
  RealContext.prototype.createMediaStreamSource = function (stream) {
    const node = createSource.call(this, stream);
    sources.set(node, stream);
    return node;
  };
  const outputs = new Map();
  const destinations = new Set();
  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  let routeChanges = 0;
  AudioNode.prototype.connect = function (node, ...args) {
    if (!outputs.has(this)) outputs.set(this, new Set());
    outputs.get(this).add(node);
    if (node === this.context.destination) {
      destinations.add(this);
      routeChanges++;
    }
    return connect.call(this, node, ...args);
  };
  AudioNode.prototype.disconnect = function (...args) {
    if (args[0] instanceof AudioNode) outputs.get(this)?.delete(args[0]);
    else outputs.delete(this);
    if (destinations.delete(this)) routeChanges++;
    return disconnect.apply(this, args);
  };
  const resets = new Map();
  const src = Object.getOwnPropertyDescriptor(
    HTMLMediaElement.prototype,
    "srcObject",
  );
  Object.defineProperty(HTMLMediaElement.prototype, "srcObject", {
    ...src,
    set(value) {
      resets.set(this, (resets.get(this) ?? 0) + 1);
      src.set.call(this, value);
    },
  });
  const trackChanges = new Map();
  for (const method of ["addTrack", "removeTrack"]) {
    const original = MediaStream.prototype[method];
    MediaStream.prototype[method] = function (track) {
      const before = this.getTracks().includes(track);
      const result = original.call(this, track);
      if (before !== this.getTracks().includes(track)) {
        trackChanges.set(this, (trackChanges.get(this) ?? 0) + 1);
      }
      return result;
    };
  }
  const plays = new Map();
  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (...args) {
    plays.set(this, (plays.get(this) ?? 0) + 1);
    return play.apply(this, args);
  };
  const messages = [];
  const send = RTCDataChannel.prototype.send;
  RTCDataChannel.prototype.send = function (value) {
    if (this.label === "v1/sys/signaling") {
      messages.push({
        peer: this.__meetPeer,
        bytes: Array.from(new Uint8Array(value)),
      });
    }
    return send.call(this, value);
  };
  const channels = new Set();
  const channel = RTCPeerConnection.prototype.createDataChannel;
  RTCPeerConnection.prototype.createDataChannel = function (...args) {
    const result = channel.apply(this, args);
    result.__meetPeer = this;
    channels.add(result);
    return result;
  };
  const retainedTopics = (connections) => {
    const expected = [
      "v1/rel/pub/chat",
      "v1/rel/sub/chat",
      "v1/rt/pub/reactions",
      "v1/rt/sub/reactions",
    ];
    const current = [...channels].filter(
      (channel) =>
        connections.includes(channel.__meetPeer) &&
        channel.label !== "v1/sys/signaling" &&
        channel.readyState === "open",
    );
    assert(
      current.length === expected.length &&
        expected.every(
          (label) =>
            current.filter((channel) => channel.label === label).length === 1,
        ),
      "both topic publisher and subscription intents remain bound exactly once",
    );
    return current;
  };
  const rejections = [];
  addEventListener("unhandledrejection", (event) =>
    rejections.push(String(event.reason)),
  );
  const receiverPeers = () =>
    [...peers].filter((peer) =>
      peer
        .getTransceivers()
        .some(
          ({ direction, receiver }) =>
            direction !== "sendonly" &&
            direction !== "inactive" &&
            receiver.track.kind === "video",
        ),
    );
  const remoteVideos = (connections = receiverPeers()) =>
    [...document.querySelectorAll("video")].filter((element) =>
      element.srcObject
        ?.getVideoTracks()
        .some((track) =>
          connections.some((peer) =>
            peer.getReceivers().some((receiver) => receiver.track === track),
          ),
        ),
    );
  const audioNodes = () =>
    [...destinations].filter((node) => node instanceof AnalyserNode);
  const reaches = (source, destination, visited = new Set()) => {
    if (source === destination) return true;
    if (visited.has(source)) return false;
    visited.add(source);
    return [...(outputs.get(source) ?? [])].some((next) =>
      reaches(next, destination, visited),
    );
  };
  const audible = (connections = receiverPeers()) =>
    audioNodes().some((node) => {
      const currentSource = [...sources].some(
        ([source, stream]) =>
          reaches(source, node) &&
          stream
            .getAudioTracks()
            .some((track) =>
              connections.some((peer) =>
                peer
                  .getReceivers()
                  .some((receiver) => receiver.track === track),
              ),
            ),
      );
      if (!currentSource) return false;
      const data = new Float32Array(node.frequencyBinCount);
      node.getFloatFrequencyData(data);
      const bin = Math.round((700 * node.fftSize) / node.context.sampleRate);
      return (
        node.context.state === "running" &&
        Math.max(...data.slice(bin - 2, bin + 3)) > -40
      );
    });
  const currentVideo = (connections = receiverPeers()) =>
    remoteVideos(connections).find(
      (element) => element.videoWidth > 0 && !element.paused,
    );
  const observeContinuity = (videos, connections) => {
    const violations = new Set();
    const streams = new Set(videos.map((element) => element.srcObject));
    for (const [source, stream] of sources) {
      if (audioNodes().some((node) => reaches(source, node)))
        streams.add(stream);
    }
    const changes = new Map(
      [...streams].map((stream) => [stream, trackChanges.get(stream) ?? 0]),
    );
    const removals = new MutationObserver((records) => {
      for (const record of records) {
        for (const removed of record.removedNodes) {
          if (
            videos.some((video) => removed === video || removed.contains(video))
          ) {
            violations.add("remote video detached from DOM");
          }
        }
      }
    });
    removals.observe(document.body, { childList: true, subtree: true });
    const listeners = [];
    const listen = (target, event, label) => {
      const listener = () => violations.add(label);
      target.addEventListener(event, listener);
      listeners.push(() => target.removeEventListener(event, listener));
    };
    for (const stream of streams) {
      for (const event of ["addtrack", "removetrack"]) {
        listen(stream, event, "remote stream tracks changed");
      }
      for (const track of stream.getTracks()) {
        for (const event of ["ended", "mute"]) {
          listen(track, event, `remote track ${event}`);
        }
      }
    }
    const frameTimes = new Map(
      videos.map((video) => [video, performance.now()]),
    );
    const callbacks = new Map();
    let active = true;
    const watchFrames = (video) => {
      callbacks.set(
        video,
        video.requestVideoFrameCallback((now) => {
          if (!active) return;
          if (now - frameTimes.get(video) > 1000) {
            violations.add("remote video stalled during topic activity");
          }
          frameTimes.set(video, now);
          watchFrames(video);
        }),
      );
    };
    for (const video of videos) {
      for (const event of ["pause", "emptied", "waiting", "stalled", "abort"]) {
        listen(video, event, `remote video ${event}`);
      }
      watchFrames(video);
    }
    const sample = () => {
      for (const [stream, before] of changes) {
        if ((trackChanges.get(stream) ?? 0) !== before) {
          violations.add("remote stream tracks changed");
        }
      }
      if (!audible(connections))
        violations.add("remote audio stopped during topic activity");
      for (const video of videos) {
        if (performance.now() - frameTimes.get(video) > 1000) {
          violations.add("remote video stalled during topic activity");
        }
      }
    };
    const timer = setInterval(sample, 25);
    return {
      assert() {
        sample();
        assert(
          violations.size === 0,
          `continuous playback: ${[...violations]}`,
        );
      },
      stop() {
        active = false;
        clearInterval(timer);
        removals.disconnect();
        for (const remove of listeners) remove();
        for (const [video, callback] of callbacks)
          video.cancelVideoFrameCallback(callback);
      },
    };
  };
  let baseline;
  const mark = () => {
    baseline?.monitor.stop();
    const videos = remoteVideos();
    assert(videos.length > 0, "real remote video mounted");
    baseline = {
      videos: videos.map((element) => ({
        element,
        stream: element.srcObject,
        resets: resets.get(element),
        plays: plays.get(element),
        frames: element.getVideoPlaybackQuality().totalVideoFrames,
      })),
      peers: receiverPeers(),
      monitor: observeContinuity(videos, receiverPeers()),
      topics: retainedTopics(receiverPeers()),
      routeChanges,
      messageIndex: messages.length,
    };
    return messages
      .filter(({ peer }) => baseline.peers.includes(peer))
      .map(({ bytes }) => bytes);
  };
  const continuity = async () => {
    const before = baseline;
    assert(before, "continuity baseline exists");
    try {
      before.monitor.assert();
      return await checkContinuity(before);
    } finally {
      before.monitor.stop();
    }
  };
  const checkContinuity = async (before) => {
    await wait(
      () =>
        before.videos.every(
          ({ element, frames }) =>
            element.getVideoPlaybackQuality().totalVideoFrames > frames + 3,
        ) && audible(),
      "advancing video and decoded current tone",
    );
    assert(
      before.peers.length === receiverPeers().length &&
        before.peers.every((peer) => receiverPeers().includes(peer)),
      "UI traffic does not replace Agent transport",
    );
    const topics = retainedTopics(before.peers);
    assert(
      topics.length === before.topics.length &&
        before.topics.every((channel) => topics.includes(channel)),
      "UI traffic retains the same topic registrations",
    );
    assert(
      routeChanges === before.routeChanges && audioNodes().length === 1,
      "UI traffic does not reset or duplicate audio routes",
    );
    for (const video of before.videos) {
      assert(
        video.element.isConnected &&
          video.element.srcObject === video.stream &&
          resets.get(video.element) === video.resets &&
          plays.get(video.element) === video.plays,
        "UI traffic cannot detach/restart remote playback even temporarily",
      );
    }
    before.monitor.assert();
    assert(rejections.length === 0, `no unhandled rejection: ${rejections}`);
    return messages
      .slice(before.messageIndex)
      .filter(({ peer }) => before.peers.includes(peer))
      .map(({ bytes }) => bytes);
  };
  const ready = async () => {
    await wait(
      () => currentVideo() && audible(),
      "real Meet video and automatic decoded audio",
    );
    assert(audioNodes().length === 1, "one audible output route");
    assert(
      !/Enable audio|Retry playback|Unlock audio/i.test(
        document.body.innerText,
      ),
      "no playback unlock UI",
    );
    return true;
  };
  const recovered = async (count) => {
    const replacements = () =>
      receiverPeers()
        .slice(count)
        .filter((peer) => peer.connectionState === "connected");
    await wait(
      () => currentVideo(replacements()) && audible(replacements()),
      "new receiver decodes automatically without repair gesture",
    );
    const video = currentVideo(replacements());
    const frames = video.getVideoPlaybackQuality().totalVideoFrames;
    await wait(
      () =>
        video.getVideoPlaybackQuality().totalVideoFrames > frames + 5 &&
        audible(replacements()),
      "recovered receiver keeps advancing",
    );
    retainedTopics(replacements());
    assert(
      audioNodes().length === 1,
      "replacement has no duplicate audio output",
    );
    assert(
      rejections.length === 0,
      `no stale rejected completion: ${rejections}`,
    );
    return true;
  };
  window.__meet = {
    assert,
    wait,
    contexts,
    blockedResumes: () => blockedResumes,
    RealContext,
    ready,
    mark,
    continuity,
    recovered,
    receiverPeers,
    audioNodes,
    audible,
    currentVideo,
    rejections,
  };
  return true;
})();
