import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { createCaptureSource } from "@pulsebeam/web";
import { useAgent, type Agent, type LogLevel } from "@pulsebeam/react";

async function until(predicate: () => boolean): Promise<void> {
  for (let i = 0; i < 100; i++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("owned Agent observation timed out");
}

export async function runOwnershipContract() {
  const track = document
    .createElement("canvas")
    .captureStream(1)
    .getVideoTracks()[0];
  const source = createCaptureSource(track, "video");
  let left: Agent | null = null;
  let right: Agent | null = null;
  let changeToken!: () => void;
  let changeEndpoint!: () => void;
  let changeLogging!: () => void;
  let changeTopology!: () => void;
  let firstRenderAvailable = false;
  function Probe() {
    const [token, setToken] = useState("owned-token-a");
    const [endpoint, setEndpoint] = useState(location.origin);
    const [logging, setLogging] = useState<LogLevel>("warn");
    const [localVideos, setLocalVideos] = useState(1);
    changeToken = () => setToken("owned-token-b");
    changeEndpoint = () => setEndpoint(`${location.origin}/replacement`);
    changeLogging = () => setLogging("debug");
    changeTopology = () => setLocalVideos(2);
    left = useAgent({
      endpoint,
      token,
      topology: { localVideos },
      logging: { level: logging },
    });
    right = useAgent({
      endpoint: location.origin,
      token: "independent-token",
    });
    firstRenderAvailable = true;
    return (
      <span>
        {left.connection} / {right.connection}
      </span>
    );
  }
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  let unmounted = false;
  try {
    root.render(
      <StrictMode>
        <Probe />
      </StrictMode>,
    );
    await until(() => left !== null && right !== null);
    const first = left!;
    const independent = right!;
    const firstVideo = first.local.video("camera");
    const secondVideo = independent.local.video("camera");
    firstVideo.setSource(source);
    secondVideo.setSource(source);
    const secondSlot = independent.local.video("screen");
    let defaultCapacityReached = false;
    try {
      independent.local.video("third");
    } catch (error) {
      defaultCapacityReached =
        error instanceof RangeError && error.message.includes("third");
    }
    const shared =
      first !== independent &&
      firstVideo !== secondVideo &&
      firstVideo.source === source &&
      secondVideo.source === source &&
      secondSlot.kind === "video" &&
      defaultCapacityReached;
    firstVideo.setEnabled(false);
    await new Promise((resolve) => setTimeout(resolve, 30));
    const independentEnablement =
      !firstVideo.enabled &&
      secondVideo.enabled &&
      track.enabled &&
      firstVideo.source === source;
    first.disconnect();
    const disconnectRetainsSource =
      firstVideo.source === source && track.readyState === "live";

    changeToken();
    await until(() => left === first);
    await new Promise((resolve) => setTimeout(resolve, 30));
    const renewedWithoutReplacing = left === first && right === independent;

    changeLogging();
    await new Promise((resolve) => setTimeout(resolve, 30));
    const loggingKeepsSession = left === first;

    changeTopology();
    await until(() => left !== null && left !== first);
    const resized = left!;
    const topologyReplacement =
      first.connection === "disconnected" &&
      resized.local.video("camera").kind === "video" &&
      resized.local.video("screen").kind === "video";

    changeEndpoint();
    await until(() => left !== null && left !== resized);
    const replacedConstruction =
      topologyReplacement &&
      resized.connection === "disconnected" &&
      right === independent;
    const replacement = left!;
    root.unmount();
    unmounted = true;
    const ownedCleanup =
      replacement.connection === "disconnected" && track.readyState === "live";
    track.stop();
    return {
      ownedIndependent:
        firstRenderAvailable &&
        shared &&
        independentEnablement &&
        disconnectRetainsSource,
      ownedRenewal: renewedWithoutReplacing && loggingKeepsSession,
      ownedReplacement: replacedConstruction,
      ownedCleanup: ownedCleanup && String(track.readyState) === "ended",
    };
  } finally {
    if (!unmounted) root.unmount();
    track.stop();
    host.remove();
  }
}
