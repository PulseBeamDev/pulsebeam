const mode = document.querySelector<HTMLInputElement>("#mode")!;
const video = document.querySelector<HTMLVideoElement>("video")!;
const endpoint = "http://localhost:3000?room=test&lukas"; 

async function startWHIP() {
  const pc = new RTCPeerConnection();
  const videoTrans = pc.addTransceiver("video", { direction: "sendonly" });
  const audioTrans = pc.addTransceiver("audio", { direction: "sendonly" });

  const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
  videoTrans.sender.replaceTrack(stream.getVideoTracks()[0]);
  audioTrans.sender.replaceTrack(stream.getAudioTracks()[0]);
  video.srcObject = stream;

  await start(pc);
}

async function startWHEP() {
  const pc = new RTCPeerConnection();
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });

  pc.ontrack = (event) => {
    video.srcObject = event.streams[0];
  };

  await start(pc);
}

async function start(pc: RTCPeerConnection) {
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);

  const response = await fetch(endpoint, {
    method: "POST",
    headers: {
      "Content-Type": "application/sdp",
    },
    body: offer.sdp,
  });

  if (!response.ok) {
    throw new Error(`WHEP request failed with status ${response.status}`);
  }

  const answerSdp = await response.text();

  await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
  console.log("Connection established successfully!");
}
