import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { Audio, Video, createAgent } from "@pulsebeam/react";
import type { LocalVideoTrack, PlaybackError } from "@pulsebeam/react";
import {
  createCaptureSource,
  type Agent,
  type AgentSnapshot,
} from "@pulsebeam/web";
import { RemoteCatalog } from "../../../pulsebeam-agent-web/dist/remote-catalog.js";

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
  let callbackVersion = 0;
  let updateCallback!: () => void;
  function Preview() {
    const [version, setVersion] = useState(1);
    updateCallback = () => setVersion(2);
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
    await new Promise((resolve) => setTimeout(resolve, 30));
    const playbackRetained =
      host.querySelector("video") === firstElement &&
      firstElement.srcObject === firstStream &&
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
      playbackRetained,
      playbackLatestCallback: callbackVersion === 2,
      detached,
      capturedPreview,
      audioExplicit,
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
  const context = new AudioContext();
  const audio = context
    .createMediaStreamDestination()
    .stream.getAudioTracks()[0];
  const snapshot = {
    catalog: {
      revision: 1,
      participants: [{ id: "participant", externalId: "alice" }],
      publications: [
        {
          id: "video",
          participantId: "participant",
          label: "camera",
          kind: "video",
        },
        {
          id: "audio",
          participantId: "participant",
          label: "microphone",
          kind: "audio",
        },
      ],
    },
    mapping: {
      acceptedIntentRevision: 1,
      video: [{ receiverIndex: 0, publicationId: "video" }],
      audio: [{ receiverIndex: 0, publicationId: "audio" }],
    },
    tracks: {
      video: { media: video, kind: "video" },
      audio: { media: audio, kind: "audio" },
    },
  } as unknown as AgentSnapshot;
  const fakeAgent = {
    getSnapshot: () => snapshot,
    subscribe: () => () => {},
  } as unknown as Agent;
  let videoDemand = 0;
  let audioDemand = false;
  const catalog = new RemoteCatalog(fakeAgent, 1, (videos, receiveAudio) => {
    videoDemand = videos.length;
    audioDemand = receiveAudio;
  });
  catalog.update(snapshot);
  const captured = createCaptureSource(video, "video");
  const render = (autoPlay: boolean) =>
    root.render(
      <>
        <Video source={captured} autoPlay={autoPlay} />
        <Video
          source={catalog.videoTracks[0]}
          autoPlay={autoPlay}
          style={{ width: 160, height: 120 }}
        />
        <Audio source={catalog.audioSource} autoPlay={autoPlay} />
      </>,
    );
  const originalPlay = HTMLMediaElement.prototype.play;
  let attempts = 0;
  HTMLMediaElement.prototype.play = () => {
    attempts += 1;
    return Promise.resolve();
  };
  try {
    render(false);
    await waitFor(() => {
      const elements = [
        ...host.querySelectorAll<HTMLMediaElement>("video,audio"),
      ];
      return (
        elements.length === 3 &&
        elements.every(
          (element) =>
            element.srcObject instanceof MediaStream &&
            element.srcObject.getTracks().length === 1,
        )
      );
    }, "autoplay disabled attachments");
    const disabled =
      attempts === 0 &&
      videoDemand === 1 &&
      audioDemand &&
      [...host.querySelectorAll<HTMLMediaElement>("video,audio")].every(
        (element) => !element.autoplay,
      );
    render(true);
    await waitFor(() => attempts >= 3, "autoplay enabled");
    const enabled = attempts === 3 && videoDemand === 1 && audioDemand;
    root.unmount();
    const released = videoDemand === 0 && !audioDemand;
    return {
      autoplayRespected:
        disabled &&
        enabled &&
        released &&
        video.readyState === "live" &&
        audio.readyState === "live",
    };
  } finally {
    root.unmount();
    catalog.close();
    host.remove();
    video.stop();
    audio.stop();
    await context.close();
    HTMLMediaElement.prototype.play = originalPlay;
  }
}
