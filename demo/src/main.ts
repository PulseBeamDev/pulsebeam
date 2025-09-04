const localVideo = document.getElementById("local") as HTMLVideoElement;
const remoteVideo = document.getElementById("remote") as HTMLVideoElement;
const form = document.getElementById("controls") as HTMLFormElement;
const endpointInput = document.getElementById("endpoint") as HTMLInputElement;
const toggleBtn = document.getElementById("toggle") as HTMLButtonElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;

let pc: RTCPeerConnection | null = null;
let localStream: MediaStream | null = null;

form.onsubmit = async (e) => {
  e.preventDefault();
  if (pc) {
    stop();
  } else {
    const endpoint = endpointInput.value.trim();
    if (!endpoint) return alert("Please enter a valid endpoint");
    toggleBtn.textContent = "Stop";
    await start(endpoint);
  }
};

async function start(endpoint: string) {
  pc = new RTCPeerConnection();

  // WHIP: send-only transceivers
  const videoTrans = pc.addTransceiver("video", { direction: "sendonly" });
  const audioTrans = pc.addTransceiver("audio", { direction: "sendonly" });
  localStream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
  videoTrans.sender.replaceTrack(localStream.getVideoTracks()[0]);
  audioTrans.sender.replaceTrack(localStream.getAudioTracks()[0]);
  localVideo.srcObject = localStream;

  // WHEP: recv-only transceivers
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  const remoteStream = new MediaStream();
  remoteVideo.srcObject = remoteStream;
  pc.ontrack = (e) => remoteStream.addTrack(e.track);

  pc.onconnectionstatechange = () => {
    statusEl.textContent = pc?.connectionState ?? "Disconnected";
  };
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const response = await fetch(endpoint, {
    method: "POST",
    headers: { "Content-Type": "application/sdp" },
    body: offer.sdp,
  });
  if (!response.ok) throw new Error(`request failed: ${response.status}`);
  const answerSdp = await response.text();
  await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
}

function stop() {
  pc?.close();
  localStream?.getTracks().forEach(track => track.stop());
  localVideo.srcObject = null;
  remoteVideo.srcObject = null;

  pc = null;
  localStream = null;
  toggleBtn.textContent = "Start";
  statusEl.textContent = pc?.connectionState ?? "Disconnected";
}

