(async () => {
  const { RemoteCatalog, attachRemoteVideo } = await import(
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
  const catalog = new RemoteCatalog(fakeAgent, 16, (video) => {
    wire = video;
  });
  const absent = catalog.participant("alice");
  const prepublication = absent.video("camera-0");
  const voice = absent.audio("microphone");
  const prelookup =
    catalog.participants.length === 0 &&
    catalog.videoTracks.length === 0 &&
    !prepublication.active &&
    !voice.receiving;
  catalog.update(snapshot);
  const initialHandles = catalog.videoTracks;
  const first = initialHandles[0];
  const externalIdentity =
    prelookup &&
    prepublication === initialHandles[0] &&
    catalog.participants[0] === absent &&
    absent.videoTracks[0] === prepublication &&
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
  let logicalLifetime = false;
  let logicalAudio = false;
  let independentKinds = false;
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
    stale.setReceiveOptions({ minHeight: 720 });
    logicalLifetime =
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
    logicalLifetime &&=
      catalog.videoTracks[0] === stale &&
      !wire.some(({ trackId }) => trackId === "video-0");

    let notifications = 0;
    voice.subscribe(() => notifications++);
    const wave = new Float32Array([1, 2, 3]);
    const spectrum = new Float32Array([1, 2, 3]);
    voice.readWaveform(wave);
    voice.readSpectrum(spectrum);
    const clears =
      wave.every((value) => value === 0) &&
      spectrum.every((value) => value === -Infinity);
    const mapped = (id) => {
      snapshot = {
        ...snapshot,
        mapping: {
          acceptedIntentRevision: 6,
          video: [],
          audio: [{ receiverIndex: 0, publicationId: id }],
        },
      };
      catalog.update(snapshot);
    };
    mapped("audio-1");
    const receivingWithoutMedia = voice.receiving && notifications === 1;
    const other = absent.audio("other");
    snapshot = {
      ...snapshot,
      catalog: {
        ...snapshot.catalog,
        publications: [
          ...snapshot.catalog.publications,
          { ...audioPublication, id: "audio-2", label: "other" },
        ],
      },
    };
    catalog.update(snapshot);
    mapped("audio-2");
    const reassignment =
      !voice.receiving && other.receiving && notifications === 2;
    snapshot = {
      ...snapshot,
      catalog: { revision: 4, participants: [], publications: [] },
      mapping: { acceptedIntentRevision: 7, video: [], audio: [] },
    };
    catalog.update(snapshot);
    const absentAgain =
      catalog.participants.length === 0 && !other.receiving && !stale.active;
    elements[0].style.display = "block";
    await new Promise((resolve) => setTimeout(resolve, 30));
    logicalLifetime &&= wire.length === 0 && !stale.active;
    snapshot = {
      ...snapshot,
      catalog: {
        revision: 5,
        participants: [{ id: "participant-NEW", externalId: "alice" }],
        publications: [
          {
            ...audioPublication,
            id: "audio-NEW",
            participantId: "participant-NEW",
          },
          {
            ...publications[0],
            id: "video-NEW",
            participantId: "participant-NEW",
          },
        ],
      },
      mapping: {
        acceptedIntentRevision: 8,
        video: [],
        audio: [{ receiverIndex: 0, publicationId: "audio-NEW" }],
      },
    };
    catalog.update(snapshot);
    const returned =
      catalog.participants[0] === absent &&
      catalog.audioTracks[0] === voice &&
      catalog.videoTracks[0] === stale &&
      voice.receiving;
    await until(() => wire.some(({ trackId }) => trackId === "video-NEW"));
    const policyRetained =
      stale.options.minHeight === 720 &&
      wire.find(({ trackId }) => trackId === "video-NEW")?.minHeight === 720;
    logicalLifetime &&= policyRetained;
    const self = new RemoteCatalog(fakeAgent, 1, () => {});
    self.update({
      ...snapshot,
      participantId: "participant-NEW",
      participantExternalId: "alice",
    });
    const selfExcluded =
      self.participants.length === 0 &&
      self.videoTracks.length === 0 &&
      self.audioTracks.length === 0;
    self.close();
    logicalAudio =
      clears &&
      receivingWithoutMedia &&
      reassignment &&
      absentAgain &&
      returned &&
      selfExcluded;
    independentKinds =
      policyRetained &&
      absent.video("microphone") !== voice &&
      absent.audio("camera-0") !== stale;
    catalog.close();
    voice.readWaveform(wave.fill(9));
    voice.readSpectrum(spectrum.fill(9));
    let lookupClosed = false;
    try {
      absent.audio("new");
    } catch {
      lookupClosed = true;
    }
    logicalAudio &&=
      !voice.receiving &&
      wave.every((value) => value === 0) &&
      spectrum.every((value) => value === -Infinity) &&
      lookupClosed;
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
    logicalLifetime,
    logicalAudio,
    independentKinds,
  };
})();
