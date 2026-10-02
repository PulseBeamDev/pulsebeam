import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { createCaptureSource } from "@pulsebeam/web";
import { useAgent, type Agent } from "@pulsebeam/react";

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
  function Probe() {
    const [token, setToken] = useState("owned-token-a");
    const [endpoint, setEndpoint] = useState(location.origin);
    changeToken = () => setToken("owned-token-b");
    changeEndpoint = () => setEndpoint(`${location.origin}/replacement`);
    left = useAgent({ endpoint, token, topology: { localVideos: 1 } });
    right = useAgent({
      endpoint: location.origin,
      token: "independent-token",
      topology: { localVideos: 1 },
    });
    return (
      <span>
        {left?.getSnapshot().connection} / {right?.getSnapshot().connection}
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
    const shared =
      first !== independent &&
      firstVideo !== secondVideo &&
      firstVideo.source === source &&
      secondVideo.source === source;
    first.disconnect();
    const disconnectRetainsSource =
      firstVideo.source === source && track.readyState === "live";

    changeToken();
    await until(() => left === first);
    await new Promise((resolve) => setTimeout(resolve, 30));
    const renewedWithoutReplacing = left === first && right === independent;

    changeEndpoint();
    await until(() => left !== null && left !== first);
    const replacedConstruction =
      firstVideo.source === null &&
      first.getSnapshot().connection === "disconnected" &&
      right === independent;
    const replacement = left!;
    root.unmount();
    unmounted = true;
    const ownedCleanup =
      secondVideo.source === null &&
      replacement.getSnapshot().connection === "disconnected" &&
      track.readyState === "live";
    track.stop();
    return {
      ownedIndependent: shared && disconnectRetainsSource,
      ownedRenewal: renewedWithoutReplacing,
      ownedReplacement: replacedConstruction,
      ownedCleanup: ownedCleanup && String(track.readyState) === "ended",
    };
  } finally {
    if (!unmounted) root.unmount();
    track.stop();
    host.remove();
  }
}
