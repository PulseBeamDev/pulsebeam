import * as sfu from "./sfu.ts";

const MAX_DOWNSTREAMS = 16;

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

  function sendRpc(msg: sfu.ClientMessage) {
    rpcCh.send(sfu.ClientMessage.toBinary(msg));
  }

  rpcCh.onmessage = async (e) => {
    const msg = sfu.ServerMessage.fromBinary(new Uint8Array(e.data)).message;
    console.log(msg);
  };

  pc.ontrack = (track) => {
    console.log("ontrack", { track });
  };

  pc.addTransceiver("video", { direction: "sendonly" });
  pc.addTransceiver("audio", { direction: "sendonly" });

  for (let i = 0; i++; i < MAX_DOWNSTREAMS) {
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
  }

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
