async function start() {
  const pc = new RTCPeerConnection();
  pc.onconnectionstatechange = () => console.log(pc.connectionState);
  const offerCh = pc.createDataChannel("offer");
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const answer = await fetch(
    "http://localhost:3000?group_id=default&peer_id=peerA",
    {
      method: "POST",
      body: offer.sdp,
      headers: {
        "Content-Type": "application/sdp",
      },
    },
  ).then((r) => r.text());
  pc.setRemoteDescription({
    type: "answer",
    sdp: answer,
  });
}
