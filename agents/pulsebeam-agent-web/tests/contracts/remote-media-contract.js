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

  function videoTrack() {
    return document.createElement("canvas").captureStream().getVideoTracks()[0];
  }

  async function settle() {
    await Promise.resolve();
    await Promise.resolve();
  }

  const audio = videoTrack();
  const video = videoTrack();
  const replacement = videoTrack();
  const attachments = [];
  try {
    const agent = fakeAgent();
    const element = document.createElement("video");
    Object.defineProperty(element, "play", {
      value: () => {
        return Promise.resolve();
      },
    });

    const attachment = pulsebeam.attachRemoteMedia(agent, element, {
      publicationIds: ["audio", "video", "audio"],
    });
    attachments.push(attachment);
    const stream = element.srcObject;
    agent.publish({ audio: { media: audio } });
    const firstAvailable =
      stream instanceof MediaStream && hasTracks(stream, audio);
    agent.publish({ ignored: { media: video }, audio: { media: audio } });
    attachment.setPublicationIds(["video", "audio", "audio"]);
    const unchanged = element.srcObject === stream && hasTracks(stream, audio);

    agent.publish({ audio: { media: audio }, video: { media: video } });
    const combined = hasTracks(stream, audio, video);
    agent.publish({ audio: { media: audio }, video: { media: replacement } });
    const replaced = hasTracks(stream, audio, replacement);
    agent.publish({ audio: { media: audio } });
    const removed = hasTracks(stream, audio);
    agent.publish({ audio: { media: audio }, video: { media: replacement } });
    const restored = hasTracks(stream, audio, replacement);

    const secondElement = document.createElement("video");
    Object.defineProperty(secondElement, "play", {
      value: () => Promise.resolve(),
    });
    const second = pulsebeam.attachRemoteMedia(agent, secondElement, {
      publicationIds: ["audio"],
    });
    attachments.push(second);
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
    const blockedElement = document.createElement("video");
    let blockedPlays = 0;
    let retry;
    let blockedMessage = "";
    let recovered = false;
    Object.defineProperty(blockedElement, "play", {
      value: () => {
        blockedPlays += 1;
        return blockedPlays === 1
          ? Promise.reject(new Error("interaction required"))
          : ((recovered = true), Promise.resolve());
      },
    });
    const blocked = pulsebeam.attachRemoteMedia(blockedAgent, blockedElement, {
      publicationIds: ["audio"],
      onPlaybackBlocked: (failure, nextRetry) => {
        blockedMessage = failure.message;
        retry = nextRetry;
      },
    });
    attachments.push(blocked);
    await settle();
    const reported =
      blockedMessage === "interaction required" && typeof retry === "function";
    await retry?.();
    const retried = recovered;
    blocked.close();

    const pendingAgent = fakeAgent({ audio: { media: audio } });
    const pendingElement = document.createElement("video");
    let rejectPlay;
    let pendingBlocked = false;
    Object.defineProperty(pendingElement, "play", {
      value: () => new Promise((_, reject) => (rejectPlay = reject)),
    });
    const pending = pulsebeam.attachRemoteMedia(pendingAgent, pendingElement, {
      publicationIds: ["audio"],
      onPlaybackBlocked: () => (pendingBlocked = true),
    });
    attachments.push(pending);
    pendingAgent.publish({});
    rejectPlay?.(new Error("retired receiver rejected late"));
    await settle();
    const retiredSuppressed = !pendingBlocked;
    pendingAgent.publish({ audio: { media: replacement } });
    rejectPlay?.(new Error("current receiver blocked"));
    await settle();
    const replacementReported = pendingBlocked;
    pendingBlocked = false;
    pendingAgent.publish({ audio: { media: audio } });
    pending.close();
    rejectPlay?.(new Error("closed receiver rejected late"));
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
      pendingSuppressed:
        retiredSuppressed && replacementReported && !pendingBlocked,
    };
    return globalThis.__pulsebeamRemoteMedia;
  } finally {
    for (const attachment of attachments) attachment.close();
    for (const track of [audio, video, replacement]) track.stop();
  }
})();
