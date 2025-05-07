import * as sfu from "./sfu.ts";

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
    console.log("answer", msg);
    rpcCh.send(sfu.ClientMessage.toBinary(msg));
  }

  rpcCh.onmessage = async (e) => {
    const msg = sfu.ServerMessage.fromBinary(new Uint8Array(e.data)).message;
    console.log(msg);
    switch (msg.oneofKind) {
      case "offer":
        await pc.setRemoteDescription({ type: "offer", sdp: msg.offer });
        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);
        sendRpc({
          message: {
            oneofKind: "answer",
            answer: answer.sdp!,
          },
        });
        console.log("answered");
        break;
      case "answer":
        break;
    }
  };

  pc.ontrack = (track) => {
    console.log("ontrack", { track });
  };

  const stream = await navigator.mediaDevices.getUserMedia({ video: true });
  stream.getTracks().forEach((t) =>
    pc.addTransceiver(t, { direction: "sendonly" })
  );

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
