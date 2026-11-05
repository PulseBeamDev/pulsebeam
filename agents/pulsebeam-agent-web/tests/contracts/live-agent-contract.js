(async () => {
  const endpoint = "__DESTINATION_ENDPOINT__";
  // Test-only observation of the browser's real receivers, without a public Agent seam.
  const peers = new Set();
  const appliedAnswers = new WeakMap();
  const warnings = [];
  const mediaDiagnostics = [];
  const originalDebug = console.debug;
  console.debug = (...args) => {
    const message = args.map(String).join(" ");
    if (/Native |RTP allocation/.test(message)) {
      mediaDiagnostics.push(message.slice(0, 4096));
      if (mediaDiagnostics.length > 256) mediaDiagnostics.shift();
    }
    originalDebug.apply(console, args);
  };
  const originalWarn = console.warn;
  console.warn = (...args) => {
    warnings.push(args.map(String).join(" ").slice(0, 600));
    if (warnings.length > 16) warnings.shift();
    originalWarn.apply(console, args);
  };
  const mediaLines = (description) =>
    description?.sdp
      .split(/\r?\n/)
      .filter((line) =>
        /^(m=|a=(mid:|send|recv|inactive|rtpmap:|fmtp:|extmap:|ssrc:|rid:|simulcast:))/.test(
          line,
        ),
      );
  const transportValues = new Map();
  const transportLines = (description) =>
    description?.sdp
      .split(/\r?\n/)
      .filter((line) =>
        /^(m=|a=(mid:|group:|bundle-only|ice-ufrag:|ice-pwd:|fingerprint:|setup:|sctp-port:))/.test(
          line,
        ),
      )
      .map((line) => {
        if (!/^a=(ice-ufrag|ice-pwd|fingerprint):/.test(line)) return line;
        if (!transportValues.has(line))
          transportValues.set(line, transportValues.size);
        return `${line.slice(0, line.indexOf(":") + 1)}<${transportValues.get(line)}>`;
      });
  const localOffers = new WeakMap();
  const originalSetLocalDescription =
    RTCPeerConnection.prototype.setLocalDescription;
  RTCPeerConnection.prototype.setLocalDescription = async function (...args) {
    const result = await originalSetLocalDescription.apply(this, args);
    const offers = localOffers.get(this) ?? [];
    offers.push(transportLines(this.localDescription));
    localOffers.set(this, offers);
    return result;
  };
  const transceivers = (peer) =>
    peer.getTransceivers().map((transceiver) => ({
      mid: transceiver.mid,
      direction: transceiver.direction,
      currentDirection: transceiver.currentDirection,
      encodings: transceiver.sender.getParameters().encodings,
      rtcp: transceiver.sender.getParameters().rtcp,
    }));
  const controlConfigurations = [];
  const payloadChannels = [];
  const originalGetStats = RTCPeerConnection.prototype.getStats;
  RTCPeerConnection.prototype.getStats = () =>
    Promise.reject(new Error("connection statistics unavailable"));
  const originalCreateDataChannel =
    RTCPeerConnection.prototype.createDataChannel;
  RTCPeerConnection.prototype.createDataChannel = function (...args) {
    const channel = originalCreateDataChannel.apply(this, args);
    if (channel.label.startsWith("native/payload/")) {
      payloadChannels.push({ peer: this, channel });
    }
    if (channel.label === "v1/sys/signaling") {
      controlConfigurations.push({
        id: channel.id,
        negotiated: channel.negotiated,
        ordered: channel.ordered,
        maxRetransmits: channel.maxRetransmits,
        maxPacketLifeTime: channel.maxPacketLifeTime,
      });
    }
    return channel;
  };
  const originalAddTransceiver = RTCPeerConnection.prototype.addTransceiver;
  RTCPeerConnection.prototype.addTransceiver = function (...args) {
    peers.add(this);
    return originalAddTransceiver.apply(this, args);
  };
  const relays = new WeakMap();
  const relayFailures = [];
  const originalSetRemoteDescription =
    RTCPeerConnection.prototype.setRemoteDescription;
  RTCPeerConnection.prototype.setRemoteDescription = async function (
    description,
  ) {
    try {
      // Remove TCP alternatives so ICE cannot bypass the UDP observation path.
      const lines = description.sdp
        .split(/\r?\n/)
        .filter(
          (line) =>
            !line.startsWith("a=candidate:") ||
            line.split(/\s+/)[2]?.toLowerCase() !== "tcp",
        );
      const candidates = lines.filter((line) =>
        line.startsWith("a=candidate:"),
      );
      if (description.type !== "answer" || !lines.includes("a=ice-lite")) {
        throw new Error("packet observation requires an ICE-lite answer");
      }
      let relay = relays.get(this);
      if (!relay) {
        if (candidates.length === 0)
          throw new Error("initial answer requires server candidates");
        const destinations = candidates
          .map((line) => {
            const fields = line.split(/\s+/);
            if (
              fields[1] !== "1" ||
              fields[2].toLowerCase() !== "udp" ||
              !/^(?:\d+\.\d+\.\d+\.\d+|[a-f0-9]*:[a-f0-9:]+)$/i.test(
                fields[4],
              ) ||
              fields[7] !== "host"
            ) {
              throw new Error(`unsupported relay candidate: ${line}`);
            }
            return fields[4].includes(":") ? null : `${fields[4]}:${fields[5]}`;
          })
          .filter(Boolean);
        const destination =
          destinations.find((value) => value.startsWith("127.0.0.1:")) ??
          destinations[0];
        if (!destination)
          throw new Error("packet relay requires an IPv4 server candidate");
        // Rewrite every route, including IPv6 candidates, to the same local relay.
        const response = await fetch(
          `/__test/rtp-relay?destination=${destination}`,
        );
        if (!response.ok) throw new Error(await response.text());
        relay = await response.json();
        relays.set(this, relay);
      }
      const sdp = lines
        .map((line) => {
          if (!line.startsWith("a=candidate:")) return line;
          const fields = line.split(/\s+/);
          fields[4] = relay.address;
          fields[5] = String(relay.port);
          return fields.join(" ");
        })
        .join("\r\n");
      const result = await originalSetRemoteDescription.call(this, {
        type: description.type,
        sdp,
      });
      const history = appliedAnswers.get(this) ?? [];
      history.push({
        media: mediaLines(this.remoteDescription),
        transport: transportLines(this.localDescription),
        transceivers: transceivers(this),
      });
      appliedAnswers.set(this, history);
      return result;
    } catch (error) {
      relayFailures.push(String(error));
      throw error;
    }
  };
  const packetEvidence = async (
    peer,
    receiver,
    accept = (observation) => observation.packets >= 4,
  ) => {
    const relay = relays.get(peer);
    if (!relay) throw new Error("receiver has no packet relay");
    const ids = new Set(
      peer.remoteDescription.sdp.split(/\r?\n/).flatMap((line) => {
        const match = line.match(
          /^a=extmap:(\d+)(?:\/\w+)? http:\/\/www.webrtc.org\/experiments\/rtp-hdrext\/playout-delay$/,
        );
        return match ? [Number(match[1])] : [];
      }),
    );
    if (ids.size !== 1)
      throw new Error(`ambiguous negotiated playout extension: ${[...ids]}`);
    const extension = [...ids][0];
    const stats = await receiver.getStats();
    const inbound = [...stats.values()].filter(
      (entry) => entry.type === "inbound-rtp" && entry.kind === "video",
    );
    if (inbound.length !== 1)
      throw new Error(`expected one inbound video SSRC, got ${inbound.length}`);
    for (let attempt = 0; attempt < 100; attempt++) {
      const response = await fetch(
        `/__test/rtp-relay/${relay.id}/${inbound[0].ssrc}?extension=${extension}`,
      );
      if (!response.ok) throw new Error(await response.text());
      const observation = await response.json();
      if (accept(observation))
        return { relay: relay.id, ssrc: inbound[0].ssrc, ...observation };
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(
      `no captured RTP for relay ${relay.id}, SSRC ${inbound[0].ssrc}`,
    );
  };
  const peerForTrack = (track) =>
    [...peers].find((peer) =>
      peer.getReceivers().some((receiver) => receiver.track === track),
    );
  const { attachRemoteVideo, createAgent, VideoSource, AudioSource } =
    pulsebeam;
  const sender = await createAgent({
    endpoint,
    token: "__SENDER_TOKEN__",
  });
  const receiver = await createAgent({
    endpoint,
    token: "__RECEIVER_TOKEN__",
  });
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const until = async (predicate, label) => {
    const deadline = Date.now() + 20000;
    while (!(await predicate())) {
      if (Date.now() > deadline) {
        const agents = [sender, receiver].map((agent) => {
          const { connection, generation, failure, catalog, mapping, topics } =
            agent.snapshot();
          return { connection, generation, failure, catalog, mapping, topics };
        });
        const physical = await Promise.all(
          [...peers].map(async (peer) => ({
            connection: peer.connectionState,
            signaling: peer.signalingState,
            sctp: peer.sctp?.state,
            channels: [...(await originalGetStats.call(peer)).values()].filter(
              (entry) =>
                entry.type === "data-channel" ||
                entry.type === "outbound-rtp" ||
                entry.type === "inbound-rtp" ||
                entry.type === "media-source" ||
                (entry.type === "candidate-pair" && entry.nominated) ||
                entry.type === "codec",
            ),
            senders: peer.getSenders().map((sender) => ({
              kind: sender.track?.kind,
              state: sender.track?.readyState,
              enabled: sender.track?.enabled,
              muted: sender.track?.muted,
              settings: sender.track?.getSettings(),
              parameters: sender.getParameters(),
            })),
            media: mediaLines(peer.localDescription),
            answers: appliedAnswers.get(peer),
            transceivers: transceivers(peer),
          })),
        );
        throw new Error(
          `live media: ${label}; relay failures: ${relayFailures}; state: ${JSON.stringify(
            {
              agents,
              warnings,
              mediaDiagnostics,
              render: {
                width: video.videoWidth,
                height: video.videoHeight,
                paused: video.paused,
              },
              physical,
            },
            (_key, value) =>
              typeof value === "bigint" ? value.toString() : value,
          )}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 16;
  const paint = canvas.getContext("2d");
  let color = false;
  const painting = setInterval(() => {
    paint.fillStyle = (color = !color) ? "#20a0ff" : "#ff8020";
    paint.fillRect(0, 0, 16, 16);
  }, 100);
  const videoTrack = canvas.captureStream(10).getVideoTracks()[0];
  const source = new AudioContext();
  const audio = source.createMediaStreamDestination();
  const oscillator = source.createOscillator();
  oscillator.frequency.value = 700;
  oscillator.connect(audio);
  oscillator.start();
  const audioTrack = audio.stream.getAudioTracks()[0];
  const outputRoutes = new Set();
  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  AudioNode.prototype.connect = function (node, ...args) {
    const result = connect.call(this, node, ...args);
    if (this instanceof AnalyserNode && node === this.context.destination)
      outputRoutes.add(this);
    return result;
  };
  AudioNode.prototype.disconnect = function (...args) {
    outputRoutes.delete(this);
    return disconnect.apply(this, args);
  };
  const video = document.createElement("video");
  video.muted = true;
  video.playsInline = true;
  video.style.cssText = "width:160px;height:120px";
  document.body.append(video);
  const originalReplaceTrack = RTCRtpSender.prototype.replaceTrack;
  const originalClone = MediaStreamTrack.prototype.clone;
  const cloneSources = new WeakMap();
  const observeClone = function () {
    const clone = originalClone.call(this);
    cloneSources.set(clone, this);
    return clone;
  };
  const presentedFrom = (source, element = video) =>
    cloneSources.get(element.srcObject?.getVideoTracks()[0]) === source;
  const handoffClones = [];
  let releaseReplacement;
  let attachment;
  let handoffAttachment;
  let handoffVideo;
  let videoPublication;
  let audioPublication;
  let replacementVideoTrack;
  let steadyIterator;
  let cyclingIterator;
  let audioSelection;
  let audioPlayback;
  let retiringPublication;
  const pendingReplacements = [];
  try {
    MediaStreamTrack.prototype.clone = observeClone;
    await source.resume();
    audioSelection = await receiver.selectAudio({ automatic: true });
    audioPlayback = pulsebeam.attachRemoteAudio(receiver);
    await audioPlayback.ready;
    videoPublication = await sender
      .video("camera")
      .publish(VideoSource.fromTrack(videoTrack), { enabled: true });
    audioPublication = await sender
      .audio("voice")
      .publish(AudioSource.fromTrack(audioTrack), { enabled: true });
    await sender.connect();
    await receiver.connect();
    await until(
      () =>
        sender.connection === "connected" &&
        receiver.connection === "connected",
      "public connection",
    );
    assert(
      sender.participantId === "web-sender" &&
        receiver.participantId === "web-receiver",
      "admission identity",
    );
    assert(receiver.roomId === "public-web-contract", "room identity");
    const senderGeneration = sender.snapshot().generation;
    const receiverGeneration = receiver.snapshot().generation;
    const receiveValue = async (iterator, expected) => {
      let received;
      let failure;
      const reading = (async () => {
        while (true) {
          const update = await iterator.next();
          assert(
            update.type === "event" && update.event.type === "message",
            "ordered data stays live without gaps",
          );
          assert(
            update.event.data.length === 1 && update.event.data[0] <= expected,
            "data from a future lifetime",
          );
          if (update.event.data[0] === expected) {
            received = update.event;
            return;
          }
        }
      })().catch((error) => {
        failure = error;
      });
      await until(() => {
        if (failure) throw failure;
        return received;
      }, "current ordered payload delivery");
      await reading;
    };
    const dataReady = async (name) => {
      await until(
        () =>
          sender
            .snapshot()
            .topics.publishers.some(
              (publisher) =>
                publisher.name === name &&
                publisher.mode === "ordered" &&
                publisher.connected &&
                typeof publisher.dataTrackId === "string",
            ) &&
          receiver
            .snapshot()
            .topics.subscribers.some(
              (subscriber) =>
                subscriber.name === name &&
                subscriber.mode === "ordered" &&
                subscriber.connected &&
                subscriber.publishers > 0,
            ),
        "accepted named-data bindings",
      );
    };
    const warmOrdered = async (publisher, iterator, value, name) => {
      let received = false;
      let failure;
      const delivery = receiveValue(iterator, value).then(
        () => {
          received = true;
        },
        (error) => {
          failure = error;
        },
      );
      // Connected bindings can precede the initial live frontier. Only the warmup
      // is retried; the assertion payload below is sent once on a witnessed route.
      await until(async () => {
        if (failure) throw failure;
        if (!received) await publisher.send(Uint8Array.of(value));
        return received;
      }, "ordered live-frontier rendezvous");
      await delivery;
      await dataReady(name);
    };
    const steadyPublisher = sender.data("reuse-steady", "ordered");
    const cyclingPublisher = sender.data("reuse-cycle", "ordered");
    steadyIterator = await receiver
      .participant("web-sender")
      .data("reuse-steady", "ordered")
      .receive();
    await warmOrdered(steadyPublisher, steadyIterator, 0, "reuse-steady");
    await steadyPublisher.send(Uint8Array.of(1));
    await receiveValue(steadyIterator, 1);
    const steadyChannels = payloadChannels.filter(
      ({ channel }) => channel.readyState === "open",
    );
    assert(
      steadyChannels.length === 2,
      "one continuing data route on the two peers",
    );
    let retiredPayload;
    for (let lifetime = 0; lifetime < 3; lifetime++) {
      cyclingIterator = await receiver
        .participant("web-sender")
        .data("reuse-cycle", "ordered")
        .receive();
      // Earlier retained values on a fresh subscription are legitimate replay.
      await warmOrdered(
        cyclingPublisher,
        cyclingIterator,
        lifetime * 2,
        "reuse-cycle",
      );
      await cyclingPublisher.send(Uint8Array.of(lifetime * 2 + 1));
      await receiveValue(cyclingIterator, lifetime * 2 + 1);
      const live = payloadChannels.filter(
        ({ channel }) => channel.readyState === "open",
      );
      assert(live.length === 4, "only two live payload channels per peer");
      if (retiredPayload) {
        const replacement = live.filter(
          ({ peer, channel }) =>
            peer === retiredPayload.peer &&
            channel.id === retiredPayload.channel.id,
        );
        assert(
          replacement.length === 1 &&
            replacement[0].channel !== retiredPayload.channel,
          "same SID has a fresh real browser channel object",
        );
        assert(
          retiredPayload.peer.sctp === retiredPayload.association &&
            retiredPayload.association.state === "connected",
          "reuse keeps the original connected SCTP association",
        );
      }
      await cyclingIterator.close();
      await until(
        () =>
          live.filter(({ channel }) => channel.readyState === "closed")
            .length === 1,
        "authoritative subscription cutoff and real browser close",
      );
      const ended = live.find(({ channel }) => channel.readyState === "closed");
      retiredPayload = { ...ended, association: ended.peer.sctp };
      await until(
        () =>
          ended.channel.onmessage === null &&
          ended.channel.onopen === null &&
          ended.channel.onclose === null,
        "retired browser callbacks detached",
      );
      assert(
        live
          .filter(({ channel }) => channel !== ended.channel)
          .every(({ channel }) => channel.readyState === "open"),
        "unrelated real payload channels survive retirement",
      );
      assert(
        steadyChannels.every(({ channel }) => channel.readyState === "open"),
        "continuing ordered bindings are not replaced",
      );
      assert(
        sender.snapshot().generation === senderGeneration &&
          receiver.snapshot().generation === receiverGeneration,
        "payload reuse does not reconnect either peer",
      );
      await steadyPublisher.send(Uint8Array.of(lifetime + 2));
      await receiveValue(steadyIterator, lifetime + 2);
    }
    await until(
      () =>
        receiver
          .snapshot()
          .catalog.publications.some((p) => p.kind === "video"),
      "video discovery",
    );
    const publication = receiver
      .snapshot()
      .catalog.publications.find((p) => p.kind === "video");
    const remoteVideo = receiver.participant("web-sender").video("camera");
    attachment = attachRemoteVideo(remoteVideo, video, undefined, {
      preferences: {
        minHeight: 1,
        minFps: 1,
        priority: 100,
        playoutDelay: { minMs: 100, maxMs: 100 },
      },
    });
    await attachment.ready;
    await until(
      () =>
        receiver.snapshot().tracks[publication.id]?.media.readyState === "live",
      "mapped fixed-delay track",
    );
    const fixedTrack = receiver.snapshot().tracks[publication.id].media;
    const fixedPeer = peerForTrack(fixedTrack);
    const fixedReceiver = fixedPeer
      .getReceivers()
      .find((r) => r.track === fixedTrack);
    const decoded = async (rtpReceiver) =>
      [...(await rtpReceiver.getStats()).values()]
        .filter((r) => r.type === "inbound-rtp" && r.kind === "video")
        .reduce((n, r) => n + (r.framesDecoded ?? 0), 0);
    const frames = await decoded(fixedReceiver);
    await until(
      async () =>
        (await decoded(fixedReceiver)) > frames + 1 &&
        video.videoWidth > 0 &&
        !video.paused,
      "fresh fixed-delay decoded/renderable frames",
    );
    let fixedPresentation = video.srcObject.getVideoTracks()[0];
    assert(
      presentedFrom(fixedTrack) &&
        fixedPresentation !== fixedTrack &&
        fixedPresentation.readyState === "live",
      "element uses an owned observation clone of the current native receiver",
    );
    // Keep one committed view throughout each handoff. A final release below is
    // deliberately different: it may retire the physical receiver lifetime.
    const witnessView = async (element, label) => {
      const before = await decoded(fixedReceiver);
      await until(
        async () =>
          receiver.snapshot().tracks[publication.id]?.media === fixedTrack &&
          fixedTrack.readyState === "live" &&
          peerForTrack(fixedTrack) === fixedPeer &&
          (await decoded(fixedReceiver)) > before + 1 &&
          presentedFrom(fixedTrack, element) &&
          element.videoWidth > 0 &&
          !element.paused,
        label,
      );
      assert(
        sender.snapshot().generation === senderGeneration &&
          receiver.snapshot().generation === receiverGeneration &&
          fixedPeer.connectionState === "connected" &&
          peers.size === 2 &&
          receiver.participant("web-sender").video("camera") === remoteVideo,
        `${label}: same agents, connection and remote handle`,
      );
    };
    const deliverDuringView = async (value) => {
      await steadyPublisher.send(Uint8Array.of(value));
      await receiveValue(steadyIterator, value);
    };
    video.style.cssText =
      "position:fixed;left:30px;top:20px;width:220px;height:160px";
    attachment.setPreferences({
      minHeight: 2,
      minFps: 1,
      priority: 90,
      playoutDelay: { minMs: 100, maxMs: 100 },
    });
    await deliverDuringView(5);
    await witnessView(video, "moved and resized view");

    handoffVideo = document.createElement("video");
    handoffVideo.muted = true;
    handoffVideo.playsInline = true;
    handoffVideo.style.cssText =
      "position:fixed;left:10px;top:10px;width:160px;height:120px";
    document.body.append(handoffVideo);
    const fixedPreferences = {
      minHeight: 1,
      minFps: 1,
      priority: 100,
      playoutDelay: { minMs: 100, maxMs: 100 },
    };
    handoffAttachment = attachRemoteVideo(
      remoteVideo,
      handoffVideo,
      undefined,
      {
        preferences: fixedPreferences,
      },
    );
    await handoffAttachment.ready;
    await witnessView(handoffVideo, "overlapping presentation");
    await attachment.close();
    assert(
      video.srcObject === null && fixedPresentation.readyState === "ended",
      "old presentation releases its element and owned observation",
    );
    await deliverDuringView(6);
    await witnessView(handoffVideo, "handoff retains live receiver");

    attachment = attachRemoteVideo(remoteVideo, video, undefined, {
      preferences: fixedPreferences,
    });
    await attachment.ready;
    await witnessView(video, "remounted presentation");
    const handoffPresentation = handoffVideo.srcObject.getVideoTracks()[0];
    await handoffAttachment.close();
    handoffAttachment = undefined;
    assert(
      handoffVideo.srcObject === null &&
        handoffPresentation.readyState === "ended",
      "handoff element and owned observation released",
    );
    handoffVideo.remove();
    handoffVideo = undefined;
    await deliverDuringView(7);
    await witnessView(video, "remount retains live receiver");
    fixedPresentation = video.srcObject.getVideoTracks()[0];
    await steadyIterator.close();
    steadyIterator = undefined;

    const fixedRtp = await packetEvidence(fixedPeer, fixedReceiver);
    assert(
      fixedRtp.withExtension > 0 &&
        fixedRtp.firstValue?.join(",") === "0,160,10",
      "actual SSRC carries negotiated fixed playout extension",
    );
    const spectrum = new Float32Array(1024);
    const audible = () => {
      if (outputRoutes.size !== 1) return false;
      const analyser = [...outputRoutes][0];
      analyser.getFloatFrequencyData(spectrum);
      const bin = Math.round((700 * 2048) / source.sampleRate);
      return (
        Math.max(...spectrum.slice(bin - 2, bin + 3)) > -40 &&
        analyser.context.state === "running"
      );
    };
    await until(
      audible,
      "explicitly owned real decoded tone without application audio element",
    );
    assert(
      document.querySelector("audio") === null,
      "no application audio element",
    );
    attachment.setPreferences({ minHeight: 1, minFps: 1, priority: 100 });
    const packetsWithoutDelay = fixedRtp.packets - fixedRtp.withExtension;
    await packetEvidence(
      fixedPeer,
      fixedReceiver,
      (observation) =>
        observation.packets - observation.withExtension >=
        packetsWithoutDelay + 4,
    );
    assert(
      receiver.snapshot().tracks[publication.id].media === fixedTrack &&
        fixedTrack.readyState === "live",
      "mutable playout settings preserve the installed receiver",
    );
    const retired = fixedPeer
      .getTransceivers()
      .find((transceiver) => transceiver.receiver.track === fixedTrack);
    assert(
      retired?.mid !== null && retired?.mid !== undefined,
      "installed receiver MID",
    );
    replacementVideoTrack = canvas.captureStream(10).getVideoTracks()[0];
    await videoPublication.setSource(
      VideoSource.fromTrack(replacementVideoTrack),
    );
    videoTrack.stop();
    await attachment.close();
    assert(
      fixedPresentation.readyState === "ended",
      "attachment close ends its owned observation track",
    );
    await until(
      () => fixedTrack.readyState === "ended" && retired.mid === null,
      "retired receiver ends and its actual browser MID clears",
    );
    attachment = attachRemoteVideo(remoteVideo, video, undefined, {
      preferences: { minHeight: 1, minFps: 1, priority: 100 },
    });
    await attachment.ready;
    await until(() => {
      const track = receiver.snapshot().tracks[publication.id]?.media;
      return (
        track &&
        track !== fixedTrack &&
        track.readyState === "live" &&
        fixedTrack.readyState === "ended"
      );
    }, "released slot receives a fresh native receiver");
    const current = receiver.snapshot().tracks[publication.id].media;
    const peer = peerForTrack(current);
    const rtpReceiver = peer.getReceivers().find((r) => r.track === current);
    assert(
      peer === fixedPeer && fixedPeer.signalingState !== "closed",
      `physical receiver retirement preserves the connection; warnings: ${warnings.join("; ")}; sender generation: ${sender.snapshot().generation}; receiver generation: ${receiver.snapshot().generation}; old peer: ${fixedPeer.connectionState}/${fixedPeer.signalingState}; new peer: ${peer.connectionState}/${peer.signalingState}; transport: ${JSON.stringify({ actual: transportLines(fixedPeer.localDescription), offers: localOffers.get(fixedPeer), applied: appliedAnswers.get(fixedPeer)?.map((answer) => answer.transport) })}`,
    );
    const baseline = await decoded(rtpReceiver);
    await until(
      async () =>
        (await decoded(rtpReceiver)) > baseline + 1 &&
        presentedFrom(current) &&
        audible(),
      "fresh current-incarnation video and decoded audio",
    );
    const currentPresentation = video.srcObject.getVideoTracks()[0];
    const defaultRtp = await packetEvidence(peer, rtpReceiver);
    assert(
      defaultRtp.relay === fixedRtp.relay &&
        defaultRtp.ssrc !== fixedRtp.ssrc &&
        defaultRtp.withExtension === 0 &&
        defaultRtp.firstValue === null,
      "fresh SSRC has no obsolete playout extension",
    );
    const allocatedSender = [...peers]
      .flatMap((observedPeer) => observedPeer.getSenders())
      .find(
        (rtpSender) =>
          rtpSender.track?.kind === "video" &&
          rtpSender.track.readyState === "live",
      );
    assert(allocatedSender?.track, "connected publication has a native sender");
    const publishingPeer = [...peers].find((peer) =>
      peer.getSenders().includes(allocatedSender),
    );
    const previousVideoTransceiver = fixedPeer
      .getTransceivers()
      .find((transceiver) => transceiver.receiver.track === current);
    const previousOwnedTrack = allocatedSender.track;
    let pendingTrack;
    MediaStreamTrack.prototype.clone = function () {
      const clone = observeClone.call(this);
      if (this === replacementVideoTrack) handoffClones.push(clone);
      return clone;
    };
    RTCRtpSender.prototype.replaceTrack = function (next) {
      if (this !== allocatedSender || pendingTrack)
        return originalReplaceTrack.call(this, next);
      pendingTrack = next;
      return new Promise((resolve, reject) => {
        releaseReplacement = () =>
          originalReplaceTrack.call(this, next).then(resolve, reject);
      });
    };
    pendingReplacements.push(
      videoPublication.setSource(VideoSource.fromTrack(replacementVideoTrack)),
    );
    await until(() => pendingTrack, "native sender replacement is in flight");
    assert(
      previousOwnedTrack.readyState === "ended" &&
        pendingTrack.readyState === "live",
      "replacement stops the previous owned capture before native completion",
    );
    await videoPublication.configure({ enabled: false });
    await until(
      () => !pendingTrack.enabled,
      "in-flight owned capture is muted",
    );
    assert(
      replacementVideoTrack.enabled,
      "mute does not change caller capture",
    );
    pendingReplacements.push(
      videoPublication.setSource(VideoSource.fromTrack(replacementVideoTrack)),
    );
    await Promise.resolve();
    retiringPublication = videoPublication.close();
    await until(
      () => pendingTrack.readyState === "ended",
      "release retires in-flight owned capture",
    );
    assert(
      pendingTrack.readyState === "ended" &&
        replacementVideoTrack.readyState === "live" &&
        handoffClones.length === 1,
      "release immediately stops only owned capture and fences queued cloning",
    );
    videoPublication = await sender
      .video("camera")
      .publish(VideoSource.fromTrack(replacementVideoTrack), { enabled: true });
    assert(
      handoffClones.length === 1,
      "reused slot waits for the old operation",
    );
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    const release = releaseReplacement;
    releaseReplacement = undefined;
    await release();
    await retiringPublication;
    retiringPublication = undefined;
    await Promise.all(pendingReplacements);
    await until(
      () =>
        handoffClones.length === 2 &&
        handoffClones[1].readyState === "live" &&
        handoffClones[1].enabled &&
        [...peers].some((observedPeer) =>
          observedPeer
            .getSenders()
            .some((rtpSender) => rtpSender.track === handoffClones[1]),
        ),
      "fresh owner installs its capture after the stale native completion",
    );
    MediaStreamTrack.prototype.clone = observeClone;
    const previousAudio = receiver
      .snapshot()
      .catalog.publications.find((publication) => publication.kind === "audio");
    const previousAudioTrack =
      receiver.snapshot().tracks[previousAudio.id].media;
    const previousAudioTransceiver = fixedPeer
      .getTransceivers()
      .find((transceiver) => transceiver.receiver.track === previousAudioTrack);
    await audioPublication.close();
    await until(
      () =>
        !receiver
          .snapshot()
          .catalog.publications.some(
            (publication) => publication.id === previousAudio.id,
          ) &&
        previousAudioTrack.readyState === "ended" &&
        previousAudioTransceiver.mid === null &&
        outputRoutes.size === 0,
      "retired audio source ends its receiver and owned playback route",
    );
    audioPublication = await sender
      .audio("voice")
      .publish(AudioSource.fromTrack(audioTrack), { enabled: true });
    await until(() => {
      const snapshot = receiver.snapshot();
      const fresh = (kind, oldId) => {
        const publication = snapshot.catalog.publications.find(
          (publication) =>
            publication.kind === kind && publication.id !== oldId,
        );
        return (
          publication &&
          snapshot.tracks[publication.id]?.media.readyState === "live"
        );
      };
      return fresh("video", publication.id) && fresh("audio", previousAudio.id);
    }, "both re-added named publications have fresh actual receivers");
    const readded = receiver.snapshot();
    const freshVideo = readded.catalog.publications.find(
      (publication) => publication.kind === "video",
    );
    const freshAudio = readded.catalog.publications.find(
      (publication) => publication.kind === "audio",
    );
    const freshVideoTrack = readded.tracks[freshVideo.id].media;
    const freshAudioTrack = readded.tracks[freshAudio.id].media;
    assert(
      freshVideoTrack !== current &&
        freshAudioTrack !== previousAudioTrack &&
        peerForTrack(freshVideoTrack) === fixedPeer &&
        peerForTrack(freshAudioTrack) === fixedPeer &&
        [...peers].find((peer) =>
          peer.getSenders().some((sender) => sender.track === handoffClones[1]),
        ) === publishingPeer &&
        peers.size === 2 &&
        sender.snapshot().generation === senderGeneration &&
        receiver.snapshot().generation === receiverGeneration,
      "publication churn creates fresh source-bound objects on the same two actual peers",
    );
    const freshReceiver = fixedPeer
      .getReceivers()
      .find((receiver) => receiver.track === freshVideoTrack);
    const freshFrames = await decoded(freshReceiver);
    await until(
      async () =>
        (await decoded(freshReceiver)) > freshFrames + 1 &&
        audible() &&
        presentedFrom(freshVideoTrack) &&
        video.videoWidth > 0 &&
        !video.paused,
      "re-added sources deliver fresh decoded/renderable video and real decoded tone",
    );
    const freshRtp = await packetEvidence(fixedPeer, freshReceiver);
    assert(
      freshRtp.relay === defaultRtp.relay &&
        freshRtp.ssrc !== defaultRtp.ssrc &&
        current.readyState === "ended" &&
        previousVideoTransceiver.mid === null &&
        previousAudioTrack.readyState === "ended" &&
        previousAudioTransceiver.mid === null &&
        fixedPresentation.readyState === "ended" &&
        currentPresentation.readyState === "ended" &&
        video.srcObject.getVideoTracks()[0] !== currentPresentation &&
        outputRoutes.size === 1,
      "old source observations remain terminal while replacement media flows",
    );
    for (const observedPeer of peers) {
      const offers = localOffers.get(observedPeer);
      const initial = offers?.[0];
      assert(
        initial?.filter((line) => line.startsWith("m=")).length === 1 &&
          initial.some((line) => line.startsWith("m=application ")),
        "bootstrap reserves only the SCTP transport anchor",
      );
      const transport = (offer) =>
        JSON.stringify(
          [
            ...new Set(
              offer.filter((line) =>
                /^a=(ice-ufrag:|ice-pwd:|fingerprint:|setup:)/.test(line),
              ),
            ),
          ].sort(),
        );
      assert(
        offers.every((offer) => transport(offer) === transport(initial)),
        "media allocation and retirement preserve the bootstrap ICE/DTLS transport",
      );
    }
    assert(
      controlConfigurations.length === 2 &&
        controlConfigurations.every(
          (channel) =>
            channel.id === 0 &&
            channel.negotiated &&
            channel.ordered &&
            channel.maxRetransmits === null &&
            channel.maxPacketLifeTime === null,
        ),
      "in-place receiver replacement keeps the two negotiated reliable ordered SID 0 channels",
    );
    await attachment.close();
    assert(video.srcObject === null, "element cleanup");
    await receiver.close();
    await until(() => outputRoutes.size === 0, "owned audio routes released");
    await sender.close();
    assert(
      replacementVideoTrack.readyState === "live" &&
        audioTrack.readyState === "live" &&
        handoffClones.every((clone) => clone.readyState === "ended"),
      "SDK stops owned media, not caller capture",
    );
    return true;
  } finally {
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    MediaStreamTrack.prototype.clone = originalClone;
    await releaseReplacement?.();
    await Promise.allSettled([
      steadyIterator?.close(),
      cyclingIterator?.close(),
      attachment?.close(),
      handoffAttachment?.close(),
      videoPublication?.close(),
      audioPublication?.close(),
      audioPlayback?.close(),
      audioSelection?.close(),
      retiringPublication,
      ...pendingReplacements,
      sender.close(),
      receiver.close(),
    ]);
    handoffVideo?.remove();
    video.remove();
    clearInterval(painting);
    videoTrack.stop();
    replacementVideoTrack?.stop();
    audioTrack.stop();
    await source.close();
    console.warn = originalWarn;
    console.debug = originalDebug;
    RTCPeerConnection.prototype.getStats = originalGetStats;
    RTCPeerConnection.prototype.createDataChannel = originalCreateDataChannel;
    RTCPeerConnection.prototype.addTransceiver = originalAddTransceiver;
    RTCPeerConnection.prototype.setRemoteDescription =
      originalSetRemoteDescription;
    AudioNode.prototype.connect = connect;
    AudioNode.prototype.disconnect = disconnect;
  }
})();
