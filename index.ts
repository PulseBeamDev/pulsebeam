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
  const rpcCh = pc.createDataChannel("pulsebeam::sfu");

  rpcCh.onmessage = (e) => {
    const msg = sfu.ServerMessage.fromBinary(e.data).message;
    console.log(msg);
    switch (msg.oneofKind) {
      case "offer":
        break;
      case "answer":
        break;
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
