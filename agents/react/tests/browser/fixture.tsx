import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Video,
  useAgent,
  useUserMedia,
  useMediaDevices,
  type Agent,
  type CaptureResult,
  type PlaybackError,
} from "@pulsebeam/react";
import { nativeCaptureTrack } from "@pulsebeam/web";
import { runConcurrencyContract } from "./concurrency.js";

declare global {
  var __pulsebeamReactObservation: Promise<boolean> | undefined;
}

globalThis.__pulsebeamReactObservation = (async () => {
  const assert = (condition: unknown, label: string) => {
    if (!condition) throw new Error(label);
  };
  const until = async (predicate: () => boolean, label: string) => {
    const deadline = Date.now() + 5000;
    while (!predicate()) {
      if (Date.now() > deadline) throw new Error(`React smoke: ${label}`);
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  assert(
    (await runConcurrencyContract()).committedRenderIsolation,
    "abandoned React render cannot change committed capture or playback callbacks",
  );
  let agent!: Agent;
  let capture!: CaptureResult;
  let updateToken!: () => void;
  let replaceSession!: () => void;
  let failure: PlaybackError | undefined;
  let deviceReady = false;
  function Room() {
    const [token, setToken] = useState("first");
    const [endpoint, setEndpoint] = useState(location.origin);
    agent = useAgent({ endpoint, token, topology: { localVideos: 1 } });
    capture = useUserMedia({ video: true, audio: false });
    deviceReady = useMediaDevices().state === "ready";
    updateToken = () => setToken("renewed");
    replaceSession = () => setEndpoint(`${location.origin}/replacement`);
    const camera = agent.local.video("camera");
    useEffect(() => {
      camera.setSource(capture.videoTrack);
      return () => camera.setSource(null);
    }, [camera, capture.videoTrack]);
    return (
      <section>
        <output>
          {token}:{String(agent.getSnapshot().desiredRevision)}
        </output>
        <Video
          source={camera}
          mirror
          onPlaybackError={(next) => {
            failure = next;
          }}
        />
      </section>
    );
  }
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const originalPlay = HTMLMediaElement.prototype.play;
  const originalCapture = navigator.mediaDevices.getUserMedia;
  let mounted = true;
  let release!: () => void;
  let late: MediaStream | undefined;
  let pending: Promise<boolean> | undefined;
  try {
    root.render(
      <StrictMode>
        <Room />
      </StrictMode>,
    );
    await until(
      () => !!capture && deviceReady && !!host.querySelector("output"),
      "committed hooks and devices",
    );
    await capture.request();
    await until(
      () =>
        capture.state === "active" && !!host.querySelector("video")?.srcObject,
      "capture subscription renders preview",
    );
    const firstAgent = agent;
    const camera = agent.local.video("camera");
    const element = host.querySelector("video")!;
    const track = nativeCaptureTrack(capture.videoTrack!, "video");
    await until(
      () => element.videoWidth > 0 && !element.paused,
      "real renderable camera preview",
    );
    assert(
      (element.srcObject as MediaStream).getVideoTracks()[0] === track &&
        element.muted &&
        element.style.transform.includes("scaleX(-1)"),
      "React uses actual captured track and native preview properties",
    );
    const revision = agent.getSnapshot().desiredRevision;
    agent.setState({
      connected: false,
      topics: [{ name: "render", mode: "latest", publish: true }],
    });
    await until(
      () =>
        agent.getSnapshot().desiredRevision > revision &&
        host.querySelector("output")!.textContent ===
          `first:${agent.getSnapshot().desiredRevision}`,
      "actual Agent subscription updates React render",
    ).catch((error) => {
      throw new Error(
        `${error}: revision=${agent.getSnapshot().desiredRevision}, rendered=${host.querySelector("output")?.textContent}`,
      );
    });
    updateToken();
    await until(
      () => host.querySelector("output")!.textContent!.startsWith("renewed:"),
      "React config update",
    );
    assert(agent === firstAgent, "renewal effect retains owned Agent");

    camera.setEnabled(false);
    await until(
      () => element.srcObject === null,
      "local subscription blanks preview",
    );
    HTMLMediaElement.prototype.play = () =>
      Promise.reject(new Error("blocked React preview"));
    camera.setEnabled(true);
    await until(() => !!failure, "playback failure reaches component callback");
    HTMLMediaElement.prototype.play = originalPlay;
    await failure!.retry();
    await until(
      () => !element.paused && element.videoWidth > 0,
      "component retry restores native playback",
    );
    replaceSession();
    await until(
      () => agent !== firstAgent && firstAgent.connection === "disconnected",
      "committed replacement deactivates old owner",
    );
    assert(
      track.readyState === "live",
      "Agent replacement leaves capture hook ownership intact",
    );

    navigator.mediaDevices.getUserMedia = async function (constraints) {
      late = await originalCapture.call(this, constraints);
      return new Promise<MediaStream>((resolve) => {
        release = () => resolve(late!);
      });
    };
    pending = capture.request().then(
      () => false,
      (error) => error.code === "request-cancelled",
    );
    await until(() => !!late && !!release, "pending native capture boundary");
    root.unmount();
    mounted = false;
    release();
    assert(await pending, "late capture is cancelled after unmount");
    assert(
      late!.getTracks().every((next) => next.readyState === "ended") &&
        track.readyState === "ended",
      "unmount fences late native capture and releases owned tracks",
    );
    assert(
      element.srcObject === null &&
        host.childElementCount === 0 &&
        agent.connection === "disconnected",
      "unmount cleans effects, media attachment and Agent runtime",
    );
    return true;
  } finally {
    if (mounted) root.unmount();
    release?.();
    await pending;
    late?.getTracks().forEach((track) => track.stop());
    navigator.mediaDevices.getUserMedia = originalCapture;
    HTMLMediaElement.prototype.play = originalPlay;
    host.remove();
  }
})();
