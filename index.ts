import * as sfu from "./sfu.ts";
import "webrtc-adapter";

const MAX_DOWNSTREAMS = 9;

function generateRandomId(length: number) {
  const characters =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let result = "";
  const charactersLength = characters.length;
  for (let i = 0; i < length; i++) {
    result += characters.charAt(Math.floor(Math.random() * charactersLength));
  }
  return result;
}

export async function start() {
  const pc = new RTCPeerConnection();
  pc.onconnectionstatechange = () => console.log(pc.connectionState);
  const rpcCh = pc.createDataChannel("pulsebeam::rpc", {});
  rpcCh.binaryType = "arraybuffer";

  const videoSender = pc.addTransceiver("video", { direction: "sendonly" });
  const audioSender = pc.addTransceiver("audio", { direction: "sendonly" });

  const videoReceivers: RTCRtpTransceiver[] = [];
  const audioReceivers: RTCRtpTransceiver[] = [];

  for (let i = 0; i < MAX_DOWNSTREAMS; i++) {
    const videoReceiver = pc.addTransceiver("video", { direction: "recvonly" });
    const audioReceiver = pc.addTransceiver("audio", { direction: "recvonly" });

    videoReceivers.push(videoReceiver);
    audioReceivers.push(audioReceiver);
  }

  function sendRpc(msg: sfu.ClientMessage) {
    rpcCh.send(sfu.ClientMessage.toBinary(msg));
  }

  rpcCh.onmessage = async (e) => {
    const msg = sfu.ServerMessage.fromBinary(new Uint8Array(e.data));
    console.log(msg);
  };

  pc.ontrack = (track) => {
    console.log("ontrack", { track });
  };

  pc.onconnectionstatechange = async () => {
    console.log(pc.connectionState);
    if (pc.connectionState === "connected") {
      const stream = await navigator.mediaDevices.getUserMedia({ video: true });
      videoSender.sender.replaceTrack(stream.getVideoTracks()[0]);
    }
  };

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const answer = await fetch(
    `http://localhost:3000?room=default&participant=${generateRandomId(6)}`,
    {
      method: "POST",
      body: offer.sdp,
      headers: {
        "Content-Type": "application/sdp",
      },
    },
  ).then((r) => r.text());
  await pc.setRemoteDescription({
    type: "answer",
    sdp: answer,
  });
}

const startButton = document.getElementById("start");
startButton?.addEventListener("click", start);
