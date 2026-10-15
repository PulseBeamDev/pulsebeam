import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { Video } from "@pulsebeam/react";
import type { LocalVideoTrack, PlaybackError } from "@pulsebeam/react";
import { createAgent, createCaptureSource } from "@pulsebeam/web";

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
  let enabled = true;
  const local = {
    kind: "video" as const,
    label: "camera",
    get enabled() {
      return enabled;
    },
    setEnabled(value: boolean) {
      enabled = value;
      listeners.forEach((listener) => listener());
    },
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
  let callbackVersion = 0;
  let updateCallback!: () => void;
  let committedVersion = 0;
  function Preview() {
    const [version, setVersion] = useState(1);
    updateCallback = () => setVersion(2);
    useEffect(() => {
      committedVersion = version;
    });
    return (
      <Video
        source={local}
        mirror
        onPlaybackError={(failure) => {
          blocked = failure;
          callbackVersion = version;
        }}
      />
    );
  }
  root.render(
    <StrictMode>
      <Preview />
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
  local.setEnabled(false);
  await waitFor(
    () => firstElement.srcObject === null,
    "disabled local preview blanks",
  );
  const disabledPreview =
    host.querySelector("video") === firstElement &&
    local.source !== null &&
    first.enabled;
  local.setEnabled(true);
  await waitFor(
    () => firstElement.srcObject instanceof MediaStream,
    "enabled local preview resumes",
  );
  const resumedStream = firstElement.srcObject as MediaStream;
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
    updateCallback();
    await waitFor(() => committedVersion === 2, "callback update committed");
    const playbackRetained =
      host.querySelector("video") === firstElement &&
      firstElement.srcObject === resumedStream &&
      disabledPreview &&
      listeners.size === 1 &&
      attempts === 0;
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

    const audioWithoutUi = host.querySelector("audio") === null;
    root.unmount();
    host.remove();
    first.stop();
    second.stop();
    return {
      localPreview,
      replaced,
      playbackError,
      playbackRetained,
      playbackLatestCallback: callbackVersion === 2,
      detached,
      capturedPreview,
      audioWithoutUi,
    };
  } finally {
    HTMLMediaElement.prototype.play = originalPlay;
  }
}

export async function runAutoplayContract() {
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const video = document
    .createElement("canvas")
    .captureStream(1)
    .getVideoTracks()[0];
  const captured = createCaptureSource(video, "video");
  const agent = createAgent({
    endpoint: location.origin,
    token: "autoplay-contract",
  });
  const remote = agent.remote.participant("alice").video("camera");
  let committed: boolean | undefined;
  function Probe({ autoPlay }: { autoPlay: boolean }) {
    useEffect(() => {
      committed = autoPlay;
    });
    return (
      <>
        <Video source={captured} autoPlay={autoPlay} />
        <Video
          source={remote}
          autoPlay={autoPlay}
          style={{ width: 160, height: 120 }}
        />
      </>
    );
  }
  const originalPlay = HTMLMediaElement.prototype.play;
  let attempts = 0;
  HTMLMediaElement.prototype.play = () => {
    attempts++;
    return Promise.resolve();
  };
  try {
    root.render(<Probe autoPlay={false} />);
    await waitFor(
      () =>
        committed === false &&
        host.querySelector("video")?.srcObject instanceof MediaStream,
      "autoplay disabled commit",
    );
    const elements = [...host.querySelectorAll("video")];
    const disabled =
      attempts === 0 &&
      elements.length === 2 &&
      elements.every((element) => !element.autoplay);
    root.render(<Probe autoPlay />);
    await waitFor(
      () => committed === true && attempts === 1,
      "autoplay enabled commit",
    );
    const retained =
      elements.every(
        (element, index) => host.querySelectorAll("video")[index] === element,
      ) &&
      (elements[0].srcObject as MediaStream)?.getVideoTracks()[0] === video;
    root.unmount();
    return {
      autoplayRespected:
        disabled &&
        retained &&
        elements.every((element) => element.srcObject === null) &&
        video.readyState === "live" &&
        host.querySelector("audio") === null,
    };
  } finally {
    root.unmount();
    agent.close();
    host.remove();
    video.stop();
    HTMLMediaElement.prototype.play = originalPlay;
  }
}
