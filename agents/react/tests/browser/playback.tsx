import { StrictMode, useState, useSyncExternalStore } from "react";
import { createRoot } from "react-dom/client";
import { Video } from "@pulsebeam/react";
import type { LocalVideoTrack, PlaybackError } from "@pulsebeam/react";
import {
  createCaptureSource,
  type Agent,
  type AgentSnapshot,
} from "@pulsebeam/web";
import { RemoteCatalog } from "../../node_modules/@pulsebeam/web/dist/remote-catalog.js";

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
  const context = new AudioContext();
  const audio = context
    .createMediaStreamDestination()
    .stream.getAudioTracks()[0];
  let snapshot = {
    connection: "connected",
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
  const subscriptions = new Set<() => void>();
  const fakeAgent = {
    getSnapshot: () => snapshot,
    subscribe: (listener: () => void) => {
      subscriptions.add(listener);
      return () => subscriptions.delete(listener);
    },
  } as unknown as Agent;
  let videoDemand = 0;
  const catalog = new RemoteCatalog(fakeAgent, 1, (videos) => {
    videoDemand = videos.length;
  });
  const nativeConnect = AudioNode.prototype.connect;
  let outputRoutes = 0;
  AudioNode.prototype.connect = function (
    this: AudioNode,
    destination: AudioNode,
    ...args: unknown[]
  ) {
    if (destination === this.context.destination) outputRoutes++;
    return Reflect.apply(nativeConnect, this, [destination, ...args]);
  } as typeof nativeConnect;
  catalog.update(snapshot);
  AudioNode.prototype.connect = nativeConnect;
  const automaticRoute = outputRoutes === 1 && host.childElementCount === 0;
  const voice = catalog.participant("alice").audio("microphone");
  function Discovery() {
    useSyncExternalStore(fakeAgent.subscribe, fakeAgent.getSnapshot);
    const receiving = useSyncExternalStore(
      (listener) => voice.subscribe(listener),
      () => voice.receiving,
    );
    return (
      <output>
        {catalog.audioTracks.length}:{String(receiving)}
      </output>
    );
  }
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
        <Discovery />
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
        elements.length === 2 &&
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
      [...host.querySelectorAll<HTMLMediaElement>("video,audio")].every(
        (element) => !element.autoplay,
      );
    render(true);
    await waitFor(() => attempts >= 2, "autoplay enabled");
    const enabled = attempts === 2 && videoDemand === 1;
    await waitFor(
      () => host.querySelector("output")?.textContent === "1:true",
      "receiving subscription",
    );
    snapshot = { ...snapshot, mapping: { ...snapshot.mapping, audio: [] } };
    catalog.update(snapshot);
    subscriptions.forEach((listener) => listener());
    await waitFor(
      () => host.querySelector("output")?.textContent === "1:false",
      "mapping subscription",
    );
    snapshot = {
      ...snapshot,
      catalog: { ...snapshot.catalog, publications: [] },
    };
    catalog.update(snapshot);
    subscriptions.forEach((listener) => listener());
    await waitFor(
      () => host.querySelector("output")?.textContent === "0:false",
      "discovery subscription",
    );
    const subscriptionsUpdated = host.querySelector("audio") === null;
    root.unmount();
    const released = videoDemand === 0 && subscriptionsUpdated;
    return {
      autoplayRespected:
        disabled &&
        enabled &&
        released &&
        automaticRoute &&
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
