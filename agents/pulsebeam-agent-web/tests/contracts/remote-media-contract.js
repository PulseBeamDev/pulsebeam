(async () => {
  function trackSet(stream) {
    return new Set(stream.getTracks());
  }

  function hasTracks(stream, ...tracks) {
    const actual = trackSet(stream);
    return (
      actual.size === tracks.length &&
      tracks.every((track) => actual.has(track))
    );
  }

  function fakeAgent(initialTracks = {}) {
    let tracks = initialTracks;
    const listeners = new Set();
    return {
      getSnapshot: () => ({ tracks }),
      subscribe(listener) {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
      publish(nextTracks) {
        tracks = nextTracks;
        for (const listener of listeners) listener();
      },
    };
  }

  function audioTrack() {
    return new AudioContext()
      .createMediaStreamDestination()
      .stream.getAudioTracks()[0];
  }

  function videoTrack() {
    return document.createElement("canvas").captureStream().getVideoTracks()[0];
  }

  async function settle() {
    await Promise.resolve();
    await Promise.resolve();
  }

  const audio = audioTrack();
  const video = videoTrack();
  const replacement = videoTrack();
  const agent = fakeAgent();
  const element = document.createElement("video");
  let plays = 0;
  Object.defineProperty(element, "play", {
    value: () => {
      plays += 1;
      return Promise.resolve();
    },
  });

  const attachment = pulsebeam.attachRemoteMedia(agent, element, {
    publicationIds: ["audio", "video", "audio"],
  });
  const stream = element.srcObject;
  agent.publish({ audio: { media: audio } });
  const firstAvailable =
    stream instanceof MediaStream && hasTracks(stream, audio) && plays === 1;
  agent.publish({ ignored: { media: video }, audio: { media: audio } });
  attachment.setPublicationIds(["video", "audio", "audio"]);
  const unchanged =
    element.srcObject === stream && hasTracks(stream, audio) && plays === 1;

  agent.publish({ audio: { media: audio }, video: { media: video } });
  const combined = hasTracks(stream, audio, video) && plays === 2;
  agent.publish({ audio: { media: audio }, video: { media: replacement } });
  const replaced = hasTracks(stream, audio, replacement) && plays === 3;
  agent.publish({ audio: { media: audio } });
  const removed = hasTracks(stream, audio) && plays === 4;
  agent.publish({ audio: { media: audio }, video: { media: replacement } });
  const restored = hasTracks(stream, audio, replacement) && plays === 5;

  const secondElement = document.createElement("audio");
  Object.defineProperty(secondElement, "play", {
    value: () => Promise.resolve(),
  });
  const second = pulsebeam.attachRemoteMedia(agent, secondElement, {
    publicationIds: ["audio"],
  });
  const secondStream = secondElement.srcObject;
  const external = new MediaStream();
  element.srcObject = external;
  attachment.close();
  attachment.close();
  const isolated =
    secondStream instanceof MediaStream &&
    hasTracks(secondStream, audio) &&
    audio.readyState !== "ended" &&
    element.srcObject === external;
  second.close();
  const terminal = secondElement.srcObject === null && hasTracks(stream);

  const blockedAgent = fakeAgent({ audio: { media: audio } });
  const blockedElement = document.createElement("audio");
  let blockedPlays = 0;
  let retry;
  let blockedMessage = "";
  Object.defineProperty(blockedElement, "play", {
    value: () => {
      blockedPlays += 1;
      return blockedPlays === 1
        ? Promise.reject(new Error("interaction required"))
        : Promise.resolve();
    },
  });
  const blocked = pulsebeam.attachRemoteMedia(blockedAgent, blockedElement, {
    publicationIds: ["audio"],
    onPlaybackBlocked: (failure, nextRetry) => {
      blockedMessage = failure.message;
      retry = nextRetry;
    },
  });
  await settle();
  const reported =
    blockedMessage === "interaction required" && typeof retry === "function";
  await retry?.();
  const retried = blockedPlays === 2;
  blocked.close();

  const pendingAgent = fakeAgent({ audio: { media: audio } });
  const pendingElement = document.createElement("audio");
  let rejectPlay;
  let pendingBlocked = false;
  Object.defineProperty(pendingElement, "play", {
    value: () => new Promise((_, reject) => (rejectPlay = reject)),
  });
  const pending = pulsebeam.attachRemoteMedia(pendingAgent, pendingElement, {
    publicationIds: ["audio"],
    onPlaybackBlocked: () => (pendingBlocked = true),
  });
  pending.close();
  rejectPlay?.(new DOMException("superseded", "AbortError"));
  await settle();

  globalThis.__pulsebeamRemoteMedia = {
    firstAvailable,
    unchanged,
    combined,
    replaced,
    removed,
    restored,
    isolated,
    terminal,
    reported,
    retried,
    pendingSuppressed: !pendingBlocked,
  };
  return globalThis.__pulsebeamRemoteMedia;
})();
