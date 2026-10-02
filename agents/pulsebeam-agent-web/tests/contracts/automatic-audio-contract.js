(async () => {
  const { createAgent, createCaptureSource } = window.pulsebeam;
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const wait = async (predicate, label, diagnostics = () => "") => {
    for (let i = 0; i < 1000; i++) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(`automatic audio condition: ${label} ${diagnostics()}`);
  };
  const mappingKey = () =>
    JSON.stringify(receiver.getSnapshot().mapping, (_, value) =>
      typeof value === "bigint" ? value.toString() : value,
    );
  const RealContext = window.AudioContext;
  const contexts = [];
  const routes = new Map();
  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  const warn = console.warn;
  const warnings = [];
  const decoders = [];
  const createElement = document.createElement;
  document.createElement = function (name, ...args) {
    const element = createElement.call(this, name, ...args);
    if (name === "audio") decoders.push(element);
    return element;
  };
  console.warn = (...args) => warnings.push(args);
  window.AudioContext = class extends RealContext {
    constructor(...args) {
      super(...args);
      contexts.push(this);
      routes.set(this, new Set());
    }
  };
  AudioNode.prototype.connect = function (node, ...args) {
    if (node === this.context.destination) routes.get(this.context)?.add(this);
    return connect.call(this, node, ...args);
  };
  AudioNode.prototype.disconnect = function (...args) {
    routes.get(this.context)?.delete(this);
    return disconnect.apply(this, args);
  };
  const source = new RealContext();
  await source.resume();
  const destination = source.createMediaStreamDestination();
  const oscillator = source.createOscillator();
  oscillator.frequency.value = 700;
  oscillator.connect(destination);
  oscillator.start();
  const track = destination.stream.getAudioTracks()[0];
  const endpoint = "http://127.0.0.1:7070";
  const sender = createAgent({
    endpoint,
    token: "__SENDER_TOKEN__",
    topology: {
      localVideos: 0,
      localAudios: 1,
      remoteVideos: 0,
      remoteAudios: 0,
    },
  });
  const receiver = createAgent({
    endpoint,
    token: "__RECEIVER_TOKEN__",
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 0,
      remoteAudios: 1,
    },
  });
  const pinned = createAgent({
    endpoint,
    token: "__SECOND_RECEIVER_TOKEN__",
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 0,
      remoteAudios: 1,
    },
  });
  let removeCatalogSubscription = () => {};
  try {
    sender.local.audio("voice").setSource(createCaptureSource(track, "audio"));
    sender.connect();
    receiver.connect();
    pinned.setState({
      connected: true,
      audio: { automatic: false, pinned: [] },
    });
    await wait(
      () =>
        receiver.getSnapshot().mapping.audio.length === 1 &&
        contexts.length === 1 &&
        routes.get(contexts[0]).size === 1,
      "automatic selection and destination without lookup/UI",
    );
    assert(
      document.querySelector("audio") === null &&
        contexts[0].state === "running",
      "autoplay needs no application audio element",
    );
    assert(
      decoders.length === 1 &&
        decoders[0].muted &&
        decoders[0].hidden &&
        !decoders[0].isConnected &&
        !decoders[0].paused,
      "automatic private decoding without UI or lookup",
    );
    const publication = receiver
      .getSnapshot()
      .catalog.publications.find((entry) => entry.kind === "audio");
    await wait(
      () =>
        pinned.getSnapshot().connection === "connected" &&
        pinned.getSnapshot().mapping.acceptedIntentRevision > 0 &&
        pinned
          .getSnapshot()
          .catalog.publications.some((entry) => entry.id === publication.id),
      "opted-out Intent acknowledged with discovery",
    );
    assert(
      pinned.getSnapshot().mapping.audio.length === 0 && contexts.length === 1,
      "explicit false opts out of automatic selection",
    );
    const handle = receiver.remote.participant("web-sender").audio("voice");
    const spectrum = new Float32Array(1024);
    await wait(() => {
      handle.readSpectrum(spectrum);
      return Math.max(...spectrum) > -40;
    }, "real server decoded signal");
    assert(handle.receiving, "real mapping drives receiving");
    let catalogConsistent = true;
    let removalObserved = false;
    let republicationObserved = false;
    removeCatalogSubscription = receiver.subscribe(() => {
      const snapshot = receiver.getSnapshot();
      const participants = snapshot.catalog.participants.filter(
        (participant) =>
          participant.id !== snapshot.participantId &&
          participant.externalId !== snapshot.participantExternalId,
      );
      const publications = snapshot.catalog.publications.filter(
        (entry) =>
          entry.kind === "audio" &&
          participants.some(
            (participant) => participant.id === entry.participantId,
          ),
      );
      const discovered = receiver.remote.audioTracks;
      catalogConsistent &&=
        receiver.remote.participants.length === participants.length &&
        receiver.remote.participants.every((participant) =>
          participants.some(
            (entry) => entry.externalId === participant.externalId,
          ),
        ) &&
        discovered.length === publications.length &&
        discovered.every((audio) =>
          publications.some(
            (entry) =>
              entry.label === audio.label &&
              participants.some(
                (participant) =>
                  participant.id === entry.participantId &&
                  participant.externalId === audio.participantId,
              ),
          ),
        );
      if (!discovered.includes(handle)) removalObserved = true;
      if (removalObserved && discovered.includes(handle))
        republicationObserved = true;
    });
    pinned.setState({
      connected: true,
      audio: { automatic: false, pinned: [publication.id] },
    });
    await wait(
      () =>
        pinned.getSnapshot().mapping.audio.length === 1 &&
        contexts.length === 2 &&
        routes.get(contexts[1]).size === 1,
      "explicit pin still plays with automatic false",
    );
    const native = receiver.getSnapshot().tracks[publication.id].media;
    await contexts[0].suspend();
    const decoder = decoders[0];
    const play = decoder.play.bind(decoder);
    decoder.play = () =>
      Promise.reject(new Error("real Agent decoder autoplay failure"));
    const catalogBeforeDisconnect = receiver.getSnapshot().catalog;
    const audioBeforeDisconnect = receiver.remote.audioTracks;
    const participantsBeforeDisconnect = receiver.remote.participants;
    receiver.disconnect();
    const disconnected = receiver.getSnapshot();
    const discovered = disconnected.catalog.publications.some(
      ({ id }) => id === publication.id,
    );
    assert(
      receiver.remote.audioTracks.includes(handle) === discovered &&
        handle.receiving ===
          (discovered &&
            disconnected.mapping.audio.some(
              ({ publicationId }) => publicationId === publication.id,
            )) &&
        (disconnected.catalog !== catalogBeforeDisconnect ||
          (receiver.remote.audioTracks === audioBeforeDisconnect &&
            receiver.remote.participants === participantsBeforeDisconnect)),
      "disconnect preserves discovery membership while Catalog is unchanged",
    );
    assert(
      routes.get(contexts[0]).size === 0 &&
        decoder.paused &&
        decoder.srcObject === null,
      "disconnect immediately fences audible and private decoder routes",
    );
    await wait(
      () =>
        receiver.getSnapshot().connection === "disconnected" &&
        !handle.receiving &&
        routes.get(contexts[0]).size === 0,
      "real disconnect cleanup",
    );
    receiver.connect();
    await wait(
      () =>
        handle.receiving &&
        receiver.getSnapshot().tracks[publication.id]?.media !== native &&
        routes.get(contexts[0]).size === 1,
      "real reconnect recreates receiver without duplicate output",
    );
    assert(
      receiver.remote.participant("web-sender").audio("voice") === handle,
      "real reconnect retains handle",
    );
    await wait(
      () =>
        warnings.some((args) =>
          String(args[1]).includes("real Agent decoder autoplay failure"),
        ),
      "real automatic failure logged",
    );
    handle.readSpectrum(spectrum.fill(1));
    const mappingBeforeRecovery = mappingKey();
    assert(
      receiver.getSnapshot().connection === "connected" &&
        receiver.getSnapshot().failure === null &&
        handle.receiving &&
        spectrum.every((value) => value === -Infinity),
      "real Agent survives blocked output with Mapping intact",
    );
    let recoveryRejected = false;
    try {
      await receiver.remote.resumeAudio();
    } catch (error) {
      recoveryRejected =
        error.message === "real Agent decoder autoplay failure";
    }
    assert(
      recoveryRejected &&
        receiver.getSnapshot().connection === "connected" &&
        mappingKey() === mappingBeforeRecovery,
      "decoder recovery failure is explicit and leaves real Mapping intact",
    );
    decoder.play = play;
    await receiver.remote.resumeAudio();
    await wait(() => {
      handle.readSpectrum(spectrum);
      return Math.max(...spectrum) > -40;
    }, "real Agent audio recovers");
    assert(
      mappingKey() === mappingBeforeRecovery &&
        routes.get(contexts[0]).size === 1,
      "recovery does not change Mapping or duplicate output",
    );
    removalObserved = false;
    republicationObserved = false;
    sender.local.audio("voice").setSource(null);
    await wait(
      () => removalObserved && !handle.receiving,
      "real Agent subscription observes publication removal",
    );
    sender.local.audio("voice").setSource(createCaptureSource(track, "audio"));
    await wait(
      () => republicationObserved && handle.receiving,
      "real Agent subscription observes republication on the same handle",
    );
    assert(catalogConsistent, "Agent callbacks see current logical discovery");
    const republished = receiver
      .getSnapshot()
      .catalog.publications.find(
        (entry) => entry.kind === "audio" && entry.label === "voice",
      );
    pinned.setState({
      connected: true,
      audio: { automatic: true, pinned: [] },
    });
    await wait(
      () =>
        pinned
          .getSnapshot()
          .mapping.audio.some(
            (entry) => entry.publicationId === republished.id,
          ) && routes.get(contexts[1]).size === 1,
      "independent Agent receives republished audio",
      () =>
        JSON.stringify(
          {
            publication: republished.id,
            mapping: pinned.getSnapshot().mapping,
            publications: pinned.getSnapshot().catalog.publications,
            failure: pinned.getSnapshot().failure,
            routes: contexts.map((context) => routes.get(context).size),
            warnings,
          },
          (_, value) => (typeof value === "bigint" ? value.toString() : value),
        ),
    );
    receiver.close();
    await wait(() => contexts[0].state === "closed", "real close resources");
    assert(
      contexts[1].state === "running" &&
        routes.get(contexts[1]).size === 1 &&
        pinned.getSnapshot().connection === "connected",
      "independent Agent remains audible",
    );
    assert(
      decoders.length === 2 &&
        decoder.paused &&
        decoder.srcObject === null &&
        decoders[1].muted &&
        decoders[1].hidden &&
        !decoders[1].isConnected &&
        !decoders[1].paused,
      "closed private decoder cannot silence the independent Agent",
    );
    assert(track.readyState === "live", "SDK leaves borrowed capture live");
    return true;
  } finally {
    removeCatalogSubscription();
    sender.close();
    receiver.close();
    pinned.close();
    track.stop();
    await source.close();
    document.createElement = createElement;
    window.AudioContext = RealContext;
    AudioNode.prototype.connect = connect;
    AudioNode.prototype.disconnect = disconnect;
    console.warn = warn;
  }
})();
