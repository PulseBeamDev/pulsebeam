(async () => {
  const endpoint = "__DESTINATION_ENDPOINT__";
  // Test-only observation of the browser's real receivers, without a public Agent seam.
  const peers = new Set();
  const appliedAnswers = new WeakMap();
  const warnings = [];
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
  const originalCreateDataChannel =
    RTCPeerConnection.prototype.createDataChannel;
  RTCPeerConnection.prototype.createDataChannel = function (...args) {
    const channel = originalCreateDataChannel.apply(this, args);
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
  const {
    attachLocalTrack,
    attachRemoteVideo,
    createAgent,
    createCaptureSource,
    track,
  } = pulsebeam;
  const sender = createAgent({
    endpoint,
    token: "__SENDER_TOKEN__",
    topology: {
      localVideos: 1,
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
      remoteVideos: 1,
      remoteAudios: 1,
    },
  });
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const until = async (predicate, label) => {
    const deadline = Date.now() + 20000;
    while (!(await predicate())) {
      if (Date.now() > deadline) {
        const agents = [sender, receiver].map((agent) => {
          const { connection, generation, failure, catalog, mapping } =
            agent.getSnapshot();
          return { connection, generation, failure, catalog, mapping };
        });
        const physical = await Promise.all(
          [...peers].map(async (peer) => ({
            connection: peer.connectionState,
            signaling: peer.signalingState,
            sctp: peer.sctp?.state,
            channels: [...(await peer.getStats()).values()].filter(
              (entry) =>
                entry.type === "data-channel" ||
                entry.type === "outbound-rtp" ||
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
          `live media: ${label}; relay failures: ${relayFailures}; state: ${JSON.stringify({ agents, warnings, physical })}`,
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
  const handoffClones = [];
  let releaseReplacement;
  let attachment;
  let videoPublication;
  let audioPublication;
  let replacementVideoTrack;
  try {
    await source.resume();
    const audioPlayback = pulsebeam.attachRemoteAudio(receiver);
    await audioPlayback.ready;
    videoPublication = attachLocalTrack(sender.track(track.video("camera")), {
      source: createCaptureSource(videoTrack, "video"),
    });
    audioPublication = attachLocalTrack(sender.track(track.audio("voice")), {
      source: createCaptureSource(audioTrack, "audio"),
    });
    await Promise.all([videoPublication.ready, audioPublication.ready]);
    sender.connect();
    receiver.connect();
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
    await until(
      () =>
        receiver
          .getSnapshot()
          .catalog.publications.some((p) => p.kind === "video"),
      "video discovery",
    );
    const publication = receiver
      .getSnapshot()
      .catalog.publications.find((p) => p.kind === "video");
    const remoteVideo = receiver.remote
      .participant("web-sender")
      .track(track.video("camera"));
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
        receiver.getSnapshot().tracks[publication.id]?.media.readyState ===
        "live",
      "mapped fixed-delay track",
    );
    const fixedTrack = receiver.getSnapshot().tracks[publication.id].media;
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
    assert(
      video.srcObject.getVideoTracks()[0] === fixedTrack,
      "element uses current native receiver",
    );
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
      receiver.getSnapshot().tracks[publication.id].media === fixedTrack &&
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
      createCaptureSource(replacementVideoTrack, "video"),
    );
    videoTrack.stop();
    attachment.close();
    await until(
      () => fixedTrack.readyState === "ended" && retired.mid === null,
      "retired receiver ends and its actual browser MID clears",
    );
    attachment = attachRemoteVideo(remoteVideo, video, undefined, {
      preferences: { minHeight: 1, minFps: 1, priority: 100 },
    });
    await attachment.ready;
    await until(() => {
      const track = receiver.getSnapshot().tracks[publication.id]?.media;
      return (
        track &&
        track !== fixedTrack &&
        track.readyState === "live" &&
        fixedTrack.readyState === "ended"
      );
    }, "released slot receives a fresh native receiver");
    const current = receiver.getSnapshot().tracks[publication.id].media;
    const peer = peerForTrack(current);
    const rtpReceiver = peer.getReceivers().find((r) => r.track === current);
    assert(
      peer === fixedPeer && fixedPeer.signalingState !== "closed",
      `physical receiver retirement preserves the connection; warnings: ${warnings.join("; ")}; sender generation: ${sender.getSnapshot().generation}; receiver generation: ${receiver.getSnapshot().generation}; old peer: ${fixedPeer.connectionState}/${fixedPeer.signalingState}; new peer: ${peer.connectionState}/${peer.signalingState}; transport: ${JSON.stringify({ actual: transportLines(fixedPeer.localDescription), offers: localOffers.get(fixedPeer), applied: appliedAnswers.get(fixedPeer)?.map((answer) => answer.transport) })}`,
    );
    const baseline = await decoded(rtpReceiver);
    await until(
      async () =>
        (await decoded(rtpReceiver)) > baseline + 1 &&
        video.srcObject.getVideoTracks()[0] === current &&
        audible(),
      "fresh current-incarnation video and decoded audio",
    );
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
    const previousOwnedTrack = allocatedSender.track;
    let pendingTrack;
    MediaStreamTrack.prototype.clone = function () {
      const clone = originalClone.call(this);
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
    videoPublication.setSource(
      createCaptureSource(replacementVideoTrack, "video"),
    );
    await until(() => pendingTrack, "native sender replacement is in flight");
    assert(
      previousOwnedTrack.readyState === "ended" &&
        pendingTrack.readyState === "live",
      "replacement stops the previous owned capture before native completion",
    );
    videoPublication.setEnabled(false);
    await until(
      () => !pendingTrack.enabled,
      "in-flight owned capture is muted",
    );
    assert(
      replacementVideoTrack.enabled,
      "mute does not change caller capture",
    );
    videoPublication.setSource(
      createCaptureSource(replacementVideoTrack, "video"),
    );
    await Promise.resolve();
    await videoPublication.close();
    assert(
      pendingTrack.readyState === "ended" &&
        replacementVideoTrack.readyState === "live" &&
        handoffClones.length === 1,
      "release immediately stops only owned capture and fences queued cloning",
    );
    videoPublication = attachLocalTrack(sender.track(track.video("camera")), {
      source: createCaptureSource(replacementVideoTrack, "video"),
    });
    await videoPublication.ready;
    assert(
      handoffClones.length === 1,
      "reused slot waits for the old operation",
    );
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    const release = releaseReplacement;
    releaseReplacement = undefined;
    await release();
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
    MediaStreamTrack.prototype.clone = originalClone;
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
    attachment.close();
    assert(video.srcObject === null, "element cleanup");
    receiver.close();
    await until(() => outputRoutes.size === 0, "owned audio routes released");
    sender.close();
    assert(
      replacementVideoTrack.readyState === "live" &&
        audioTrack.readyState === "live" &&
        handoffClones.every((clone) => clone.readyState === "ended"),
      "SDK stops owned media, not caller capture",
    );
    return true;
  } finally {
    attachment?.close();
    void videoPublication?.close();
    void audioPublication?.close();
    sender.close();
    receiver.close();
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    MediaStreamTrack.prototype.clone = originalClone;
    void releaseReplacement?.();
    video.remove();
    clearInterval(painting);
    videoTrack.stop();
    replacementVideoTrack?.stop();
    audioTrack.stop();
    await source.close();
    console.warn = originalWarn;
    RTCPeerConnection.prototype.createDataChannel = originalCreateDataChannel;
    RTCPeerConnection.prototype.addTransceiver = originalAddTransceiver;
    RTCPeerConnection.prototype.setRemoteDescription =
      originalSetRemoteDescription;
    AudioNode.prototype.connect = connect;
    AudioNode.prototype.disconnect = disconnect;
  }
})();
