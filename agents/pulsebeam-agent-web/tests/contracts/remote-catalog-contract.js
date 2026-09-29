(async () => {
  const { RemoteCatalog, attachRemoteVideo, attachRemoteAudio } = await import(
    "/dist/remote-catalog.js"
  );
  const publications = Array.from({ length: 18 }, (_, index) => ({
    id: `video-${index}`,
    kind: "video",
    participantId: "participant-1",
    label: `camera-${index}`,
  }));
  const audioPublication = {
    id: "audio-1",
    kind: "audio",
    participantId: "participant-1",
    label: "microphone",
  };
  let snapshot = {
    catalog: {
      revision: 1,
      participants: [{ id: "participant-1", externalId: "alice" }],
      publications: [...publications, audioPublication],
    },
    mapping: { acceptedIntentRevision: 0, video: [], audio: [] },
    tracks: {},
  };
  const listeners = new Set();
  const fakeAgent = {
    getSnapshot: () => snapshot,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  let wire = [];
  let audioDemand = false;
  const catalog = new RemoteCatalog(fakeAgent, 16, (video, audio) => {
    wire = video;
    audioDemand = audio;
  });
  catalog.update(snapshot);
  const initialHandles = catalog.videoTracks;
  const first = initialHandles[0];
  const externalIdentity =
    initialHandles.length === 18 &&
    first.participantId === "alice" &&
    first.label === "camera-0" &&
    catalog.audioTracks[0]?.participantId === "alice" &&
    catalog.videoTracks === initialHandles;
  first.setReceiveOptions({
    minHeight: 720,
    minFps: 15,
    priority: 100,
    playoutDelay: { minMs: 3000, maxMs: 500 },
  });
  const inertPolicy = wire.length === 0;
  const prior = JSON.stringify(wire);
  let invalidRejected = false;
  try {
    first.setReceiveOptions({ minHeight: -1 });
  } catch (error) {
    invalidRejected = error instanceof TypeError;
  }
  try {
    first.setReceiveOptions({ playoutDelay: { minMs: NaN, maxMs: 1 } });
    invalidRejected = false;
  } catch (error) {
    invalidRejected &&= error instanceof TypeError;
  }
  const invalidAtomic = invalidRejected && JSON.stringify(wire) === prior;

  const host = document.createElement("div");
  document.body.append(host);
  const elements = [];
  const attachments = [];
  const warnings = [];
  const originalWarn = console.warn;
  console.warn = (...values) => warnings.push(values);
  const until = async (predicate) => {
    for (let attempt = 0; attempt < 100; attempt++) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(
      `remote catalog condition not met: ${JSON.stringify(wire)}`,
    );
  };
  const mount = (handle, height) => {
    const element = document.createElement("video");
    element.style.width = "160px";
    element.style.height = `${height}px`;
    element.style.position = "fixed";
    element.style.top = "0";
    element.style.left = "0";
    element.style.display = "block";
    host.append(element);
    elements.push(element);
    const attachment = attachRemoteVideo(handle, element);
    attachments.push(attachment);
    return { element, attachment };
  };
  let maxPhysicalHeight = false;
  let measuredVisibility = false;
  let capacity = false;
  let hiddenFloor = false;
  let mappingNotRemoval = false;
  let removedTerminal = false;
  let audioExplicit = false;
  let playbackScoped = false;
  try {
    mount(first, 120);
    await until(() =>
      wire.some(({ trackId }) => trackId === first.publicationId),
    );
    const smaller = wire.find(({ trackId }) => trackId === first.publicationId);
    const large = mount(first, 230);
    await until(
      () =>
        wire.find(({ trackId }) => trackId === first.publicationId)?.height >
        smaller.height,
    );
    maxPhysicalHeight =
      wire.filter(({ trackId }) => trackId === first.publicationId).length ===
        1 &&
      wire.find(({ trackId }) => trackId === first.publicationId)?.height >=
        Math.ceil(230 * devicePixelRatio);
    large.attachment.close();
    large.element.remove();
    await until(
      () =>
        wire.find(({ trackId }) => trackId === first.publicationId)?.height ===
        smaller.height,
    );
    const measured = elements[0];
    measured.style.height = "200px";
    await until(() => wire[0]?.height >= Math.ceil(200 * devicePixelRatio));
    const resized = wire[0].height;
    host.style.opacity = "0";
    await until(() => wire.length === 0);
    host.style.opacity = "1";
    await until(() => wire[0]?.height === resized);
    host.style.visibility = "hidden";
    await until(() => wire.length === 0);
    host.style.visibility = "visible";
    await until(() => wire[0]?.height === resized);
    measured.style.top = "-500px";
    await until(() => wire.length === 0);
    measured.style.top = "-50px";
    await until(() => wire[0]?.height === resized);
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    await until(() => wire.length === 0);
    delete document.hidden;
    document.dispatchEvent(new Event("visibilitychange"));
    await until(() => wire[0]?.height === resized);
    measuredVisibility = wire[0]?.minHeight === 720;
    for (const handle of initialHandles.slice(1, 17)) mount(handle, 100);
    await until(() => wire.length === 16 && warnings.length > 0);
    capacity =
      wire.every(({ trackId }) => trackId !== "video-16") &&
      wire.some(({ trackId }) => trackId === "video-0") &&
      warnings.some((values) => String(values[0]).includes("capacity"));
    elements[0].style.display = "none";
    await until(() => wire.some(({ trackId }) => trackId === "video-16"));
    hiddenFloor =
      !wire.some(({ trackId }) => trackId === "video-0") &&
      wire.length === 16 &&
      first.active &&
      first.participantId === "alice";
    snapshot = {
      ...snapshot,
      mapping: { acceptedIntentRevision: 5, video: [], audio: [] },
    };
    catalog.update(snapshot);
    for (const listener of listeners) listener();
    mappingNotRemoval =
      catalog.videoTracks === initialHandles &&
      first.active &&
      wire.some(({ trackId }) => trackId === "video-16");
    snapshot = {
      ...snapshot,
      catalog: {
        ...snapshot.catalog,
        revision: 2,
        publications: [...publications.slice(1), audioPublication],
      },
    };
    catalog.update(snapshot);
    for (const listener of listeners) listener();
    const stale = first;
    stale.setReceiveOptions({ minHeight: -1 });
    removedTerminal =
      !stale.active &&
      !catalog.videoTracks.includes(stale) &&
      !wire.some(({ trackId }) => trackId === "video-0");
    snapshot = {
      ...snapshot,
      catalog: {
        ...snapshot.catalog,
        revision: 3,
        publications: [...publications, audioPublication],
      },
    };
    catalog.update(snapshot);
    removedTerminal &&=
      catalog.videoTracks[0] !== stale &&
      !wire.some(({ trackId }) => trackId === "video-0");

    const context = new AudioContext();
    const voice = context
      .createMediaStreamDestination()
      .stream.getAudioTracks()[0];
    snapshot = {
      ...snapshot,
      mapping: {
        acceptedIntentRevision: 6,
        video: [],
        audio: [{ receiverIndex: 0, publicationId: "audio-1" }],
      },
      tracks: { "audio-1": { media: voice, kind: "audio" } },
    };
    const player = document.createElement("audio");
    host.append(player);
    let attempts = 0;
    player.play = () => {
      attempts += 1;
      return attempts === 1
        ? Promise.reject(new DOMException("blocked", "NotAllowedError"))
        : Promise.resolve();
    };
    const manual = attachRemoteAudio(catalog.audioSource, player, undefined, {
      autoPlay: false,
    });
    const manuallyAttached =
      attempts === 0 &&
      audioDemand &&
      player.srcObject.getAudioTracks().includes(voice);
    await manual.retryPlayback();
    const manuallyRetried = attempts === 1;
    manual.close();
    attempts = 0;
    let blocked;
    const audio = attachRemoteAudio(
      catalog.audioSource,
      player,
      (failure, retry) => {
        blocked = { failure, retry };
      },
    );
    catalog.update(snapshot);
    for (const listener of listeners) listener();
    await until(() => blocked !== undefined);
    audioExplicit =
      manuallyAttached &&
      manuallyRetried &&
      audioDemand &&
      player.srcObject instanceof MediaStream &&
      player.srcObject.getAudioTracks().includes(voice);
    await blocked.retry();
    playbackScoped = blocked.failure.message === "blocked" && attempts === 2;
    audio.close();
    audioExplicit &&=
      !audioDemand && player.srcObject === null && voice.readyState === "live";
    voice.stop();
    await context.close();
  } finally {
    delete document.hidden;
    console.warn = originalWarn;
    for (const attachment of attachments) attachment.close();
    host.remove();
    catalog.close();
  }
  return {
    externalIdentity,
    inertPolicy,
    invalidAtomic,
    maxPhysicalHeight,
    measuredVisibility,
    capacity,
    hiddenFloor,
    mappingNotRemoval,
    removedTerminal,
    audioExplicit,
    playbackScoped,
  };
})();
