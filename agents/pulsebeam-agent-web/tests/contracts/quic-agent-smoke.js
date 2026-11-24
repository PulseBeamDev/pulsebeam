(async () => {
  const { Agent, AudioSource, DataEvent, PublicationExecution, VideoSource } =
    pulsebeam;
  const { attachMedia } = await import("/sdk/dist/internal/react.js");
  const room = "quic-browser-smoke";
  const peers = new Set();
  const setLocalDescription = RTCPeerConnection.prototype.setLocalDescription;
  RTCPeerConnection.prototype.setLocalDescription = function (...args) {
    peers.add(this);
    return setLocalDescription.apply(this, args);
  };
  const agent = new Agent({
    endpoint: "__ENDPOINT__",
    token: "__TOKEN__",
    singleVideoEncoding: true,
  });
  const canvas = document.createElement("canvas");
  canvas.width = 320;
  canvas.height = 180;
  const paint = canvas.getContext("2d");
  let light = false;
  const draw = () => {
    paint.fillStyle = (light = !light) ? "#20a0ff" : "#ff8020";
    paint.fillRect(0, 0, canvas.width, canvas.height);
  };
  draw();
  const painting = setInterval(draw, 100);
  const outgoingVideo = canvas.captureStream(10).getVideoTracks()[0];
  const context = new AudioContext();
  const audioDestination = context.createMediaStreamDestination();
  const oscillator = context.createOscillator();
  oscillator.frequency.value = 700;
  oscillator.connect(audioDestination);
  oscillator.start();
  const outgoingAudio = audioDestination.stream.getAudioTracks()[0];
  const video = document.createElement("video");
  const audio = document.createElement("audio");
  video.muted = true;
  video.playsInline = true;
  audio.muted = true;
  document.body.append(video, audio);
  const analyser = context.createAnalyser();
  const muted = context.createGain();
  muted.gain.value = 0;
  analyser.connect(muted).connect(context.destination);
  let monitor;
  let failure;
  const fail = (lane) => (error) => {
    failure ??= new Error(`${lane}: ${error?.inner?.info?.message ?? error}`);
  };
  const native = agent.room(room).participant("linux-quic");
  const videoSubscription = native.video("camera").subscribe({
    visible: true,
    height: 180,
    priority: 1,
  });
  const audioSubscription = native.audio("voice").subscribe();
  const incoming = native.data("native-bytes").subscribe();
  const outgoing = agent.data("browser-bytes").publish();
  const videoPublication = agent
    .video("browser-camera")
    .publish({ enabled: true });
  const audioPublication = agent
    .audio("browser-voice")
    .publish({ enabled: true });
  const videoPipe =
    VideoSource.fromTrack(outgoingVideo).pipeTo(videoPublication);
  const audioPipe =
    AudioSource.fromTrack(outgoingAudio).pipeTo(audioPublication);
  const videoView = attachMedia(
    videoSubscription,
    "video",
    video,
    true,
    fail("video attachment"),
    fail("video playback"),
  );
  const audioView = attachMedia(
    audioSubscription,
    "audio",
    audio,
    true,
    fail("audio attachment"),
    fail("audio playback"),
  );
  let decodedFrame = false;
  let receivedData = false;
  let heard = false;
  video.requestVideoFrameCallback(() => (decodedFrame = true));
  try {
    await context.resume();
    agent.join(room);
    const receive = (async () => {
      const event = await incoming.next();
      if (!event || !DataEvent.Message.instanceOf(event)) {
        throw new Error("browser did not receive QUIC binary DATA");
      }
      const data = Array.from(event.inner.data).join(",");
      if (data !== "0,255,9") throw new Error(`unexpected QUIC DATA ${data}`);
      receivedData = true;
    })().catch(fail("DATA receive"));
    const until = Date.now() + 12000;
    let nextData = 0;
    while (Date.now() < until) {
      if (!monitor && audio.srcObject?.getAudioTracks().length) {
        monitor = context.createMediaStreamSource(audio.srcObject);
        monitor.connect(analyser);
      }
      if (monitor) {
        const samples = new Float32Array(analyser.fftSize);
        analyser.getFloatTimeDomainData(samples);
        heard ||= samples.some((value) => Math.abs(value) > 0.001);
      }
      if (
        outgoing.state().execution === PublicationExecution.Installed &&
        Date.now() > nextData
      ) {
        nextData = Date.now() + 200;
        await outgoing.send(Uint8Array.of(255, 0, 7));
      }
      if (receivedData && decodedFrame && heard) {
        await receive;
        if (failure) throw failure;
        return true;
      }
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    const rtp = [];
    for (const peer of peers) {
      const stats = await peer.getStats();
      for (const entry of stats.values()) {
        if (entry.type === "inbound-rtp" || entry.type === "outbound-rtp") {
          rtp.push({
            type: entry.type,
            kind: entry.kind,
            codec: stats.get(entry.codecId)?.mimeType,
            sent: entry.packetsSent,
            received: entry.packetsReceived,
            decoded: entry.framesDecoded,
            width: entry.frameWidth,
          });
        }
      }
    }
    throw new Error(
      `QUIC browser proof missing: data=${receivedData} video=${decodedFrame} audio=${heard}; error=${failure}; rtp=${JSON.stringify(rtp)}`,
    );
  } finally {
    RTCPeerConnection.prototype.setLocalDescription = setLocalDescription;
    clearInterval(painting);
    outgoingVideo.stop();
    outgoingAudio.stop();
    oscillator.stop();
    await Promise.allSettled([
      videoView.close(),
      audioView.close(),
      videoPipe.close(),
      audioPipe.close(),
      incoming.close(),
      agent.close(),
      context.close(),
    ]);
    video.remove();
    audio.remove();
  }
})();
