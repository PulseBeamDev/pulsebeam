import { PulsebeamClient } from "./lib.ts";

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

const SFU_URL = "http://localhost:3000";
const ROOM_NAME = "tiny-room";
const MY_PARTICIPANT_ID = generateRandomId(4);
const REMOTE_TRACK_ID_TO_SUBSCRIBE = "participantB-video"; // SFU must know this track

// --- DOM Elements ---
const connectBtn = document.getElementById(
  "connectButton",
) as HTMLButtonElement;
const disconnectBtn = document.getElementById(
  "disconnectButton",
) as HTMLButtonElement;
const subscribeBtn = document.getElementById(
  "subscribeRemoteButton",
) as HTMLButtonElement;
const localVideo = document.getElementById(
  "localVideoElement",
) as HTMLVideoElement;
const remoteVideo = document.getElementById(
  "remoteVideoElement",
) as HTMLVideoElement;
const stateDisplay = document.getElementById(
  "connectionStateDisplay",
) as HTMLSpanElement;

// --- SDK Instance ---
const client = new PulsebeamClient(SFU_URL, 1); // Max 1 downstream video slot

// --- Event Listeners & SDK Interactions ---

// Update UI based on connection state
client.connectionState.subscribe((currentState) => {
  stateDisplay.textContent = currentState;
  const isEffectivelyConnected = currentState === "connected" ||
    currentState === "rpc_ready";

  connectBtn.disabled = isEffectivelyConnected ||
    currentState === "connecting" || currentState === "joining" ||
    currentState === "signaling";
  disconnectBtn.disabled = !isEffectivelyConnected &&
    currentState !== "connecting" && currentState !== "failed";
  subscribeBtn.disabled = currentState !== "rpc_ready"; // Only enable when RPC data channel is open

  if (
    currentState === "closed" || currentState === "failed" ||
    currentState === "new"
  ) {
    localVideo.srcObject = null;
    remoteVideo.srcObject = null;
  }
});

// Display local video track
client.localVideoTrack.subscribe((track) => {
  localVideo.srcObject = track ? new MediaStream([track]) : null;
  if (track) {
    localVideo.play().catch((e) => console.warn("Local video play error:", e));
  }
});

// Display the first active remote video track
client.remoteTrackInfos.subscribe((infos) => {
  let activeRemoteTrack: MediaStreamTrack | null = null;
  for (const trackId in infos) {
    const info = infos[trackId];
    if (info?.track && info.kind === "video" && info.state === "active") {
      activeRemoteTrack = info.track;
      break; // Take the first active one
    }
  }
  remoteVideo.srcObject = activeRemoteTrack
    ? new MediaStream([activeRemoteTrack])
    : null;
  if (activeRemoteTrack) {
    remoteVideo.play().catch((e) =>
      console.warn("Remote video play error:", e)
    );
  }
});

// Log SFU errors
client.sfuErrorMessage.subscribe((errorMessage) => {
  if (errorMessage) console.error("SFU Error:", errorMessage);
});

// Connect button action
connectBtn.onclick = async () => {
  console.log("Connecting...");
  try {
    await client.connect(ROOM_NAME, MY_PARTICIPANT_ID);
    // Once connected, attempt to get and publish local media
    const stream = await navigator.mediaDevices.getUserMedia({
      video: true,
      audio: true,
    });
    const videoTrack = stream.getVideoTracks()[0];
    const audioTrack = stream.getAudioTracks()[0]; // Good practice to handle audio too
    if (videoTrack) await client.publishTrack(videoTrack);
    if (audioTrack) await client.publishTrack(audioTrack); // Publish audio
  } catch (error) {
    console.error("Connection or media publishing failed:", error);
    client.disconnect(); // Attempt cleanup if connect or getUserMedia fails
  }
};

// Disconnect button action
disconnectBtn.onclick = () => {
  console.log("Disconnecting...");
  client.disconnect();
};

// Subscribe button action
subscribeBtn.onclick = () => {
  console.log(
    `Requesting subscription to remote track: ${REMOTE_TRACK_ID_TO_SUBSCRIBE}`,
  );
  const success = client.subscribe(REMOTE_TRACK_ID_TO_SUBSCRIBE, "video");
  if (!success) {
    console.warn(
      "Failed to initiate subscription (e.g., no free slots or not connected).",
    );
  }
};
