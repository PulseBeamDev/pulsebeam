import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Audio, Video, createAgent } from "@pulsebeam/react";
import type { LocalVideoTrack, PlaybackError } from "@pulsebeam/react";
import { createCaptureSource } from "@pulsebeam/web";

const waitFor = async (predicate: () => boolean, phase: string) => {
  for (let index = 0; index < 200; index++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`React playback condition not met: ${phase}`);
};

export async function runPlaybackContract() {
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const canvas = document.createElement("canvas");
  const first = canvas.captureStream().getVideoTracks()[0];
  const second = canvas.captureStream().getVideoTracks()[0];
  let capture: LocalVideoTrack["source"] = createCaptureSource(first, "video");
  const listeners = new Set<() => void>();
  const local = {
    kind: "video" as const,
    label: "camera",
    get source() {
      return capture;
    },
    setSource(source: Parameters<LocalVideoTrack["setSource"]>[0]) {
      capture = source;
      listeners.forEach((listener) => listener());
    },
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  } satisfies LocalVideoTrack;
  let blocked: PlaybackError | undefined;
  root.render(
    <StrictMode>
      <Video
        source={local}
        mirror
        onPlaybackError={(failure) => (blocked = failure)}
      />
    </StrictMode>,
  );
  await waitFor(
    () => host.querySelector("video")?.srcObject instanceof MediaStream,
    "logical preview",
  );
  const firstElement = host.querySelector("video")!;
  const firstStream = firstElement.srcObject as MediaStream;
  const localPreview =
    firstStream.getVideoTracks()[0] === first &&
    firstElement.muted &&
    firstElement.playsInline &&
    firstElement.style.transform.includes("scaleX(-1)");
  const originalPlay = HTMLMediaElement.prototype.play;
  blocked = undefined;
  let attempts = 0;
  HTMLMediaElement.prototype.play = () => {
    attempts++;
    return attempts === 1
      ? Promise.reject(new Error("autoplay blocked"))
      : Promise.resolve();
  };
  try {
    local.setSource(createCaptureSource(second, "video"));
    await waitFor(
      () =>
        blocked !== undefined &&
        (
          host.querySelector("video")?.srcObject as MediaStream
        )?.getVideoTracks()[0] === second,
      "logical replacement",
    );
    const replaced =
      firstStream !== host.querySelector("video")?.srcObject &&
      first.readyState === "live";
    await blocked!.retry();
    const playbackError =
      blocked!.error instanceof Error &&
      blocked!.error.message === "autoplay blocked" &&
      attempts === 2;
    root.render(<Video source={null} />);
    await waitFor(
      () => host.querySelector("video") === null && listeners.size === 0,
      "logical detach",
    );
    const detached =
      firstElement.srcObject === null && second.readyState === "live";

    root.render(
      <Video source={createCaptureSource(first, "video")} autoPlay />,
    );
    await waitFor(
      () =>
        (
          host.querySelector("video")?.srcObject as MediaStream | null
        )?.getVideoTracks()[0] === first,
      "captured preview",
    );
    const capturedElement = host.querySelector("video")!;
    root.render(<Video source={null} />);
    await waitFor(
      () => host.querySelector("video") === null,
      "captured detach",
    );
    const capturedPreview =
      capturedElement.srcObject === null && first.readyState === "live";

    const agent = createAgent({
      endpoint: location.origin,
      token: "playback-contract",
      topology: { remoteVideos: 1, remoteAudios: 1 },
    });
    root.render(<Audio source={agent.remoteAudio} />);
    await waitFor(() => host.querySelector("audio") !== null, "audio");
    const audioExplicit =
      host.querySelector("audio")?.autoplay === true &&
      host.querySelector("video") === null;
    root.unmount();
    agent.close();
    host.remove();
    first.stop();
    second.stop();
    return {
      localPreview,
      replaced,
      playbackError,
      detached,
      capturedPreview,
      audioExplicit,
    };
  } finally {
    HTMLMediaElement.prototype.play = originalPlay;
  }
}
