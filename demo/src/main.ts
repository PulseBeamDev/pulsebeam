const mode = document.querySelector<HTMLInputElement>("input[name=mode]")!;
const video = document.querySelector<HTMLVideoElement>("video")!;
const trigger = document.querySelector<HTMLButtonElement>("#trigger")!;
const form = document.querySelector<HTMLFormElement>("form")!;
const endpoint = "http://localhost:3000?room=test&participant=lukas";

form.onsubmit = async (e) => {   
  e.preventDefault();
  const data = new FormData(form);
  try {
    if (data.get("mode") === "whip") {
      await startWHIP();
    } else {
      await startWHEP();
    }
  } catch (e) {
    console.error(e);
    alert(`Error: ${e}`);
  }
}

async function startWHIP() {
  console.log("Starting WHIP...");
  const pc = new RTCPeerConnection();
  const videoTrans = pc.addTransceiver("video", { direction: "sendonly" });
  const audioTrans = pc.addTransceiver("audio", { direction: "sendonly" });

  const stream = await navigator.mediaDevices.getUserMedia({
    video: true,
    audio: true,
  });
  videoTrans.sender.replaceTrack(stream.getVideoTracks()[0]);
  audioTrans.sender.replaceTrack(stream.getAudioTracks()[0]);
  video.srcObject = stream;

  await start(pc);
}

async function startWHEP() {
  console.log("Starting WHEP...");
  const pc = new RTCPeerConnection();
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });

  pc.ontrack = (event) => {
    video.srcObject = event.streams[0];
  };

  await start(pc);
}

async function start(pc: RTCPeerConnection) {
  pc.onconnectionstatechange = () => {
    console.log("Connection state:", pc.connectionState);
  };
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
