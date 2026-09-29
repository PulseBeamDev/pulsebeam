import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  useDisplayMedia,
  useMediaDevices,
  useUserMedia,
  type CaptureResult,
  type MediaDevicesResult,
} from "@pulsebeam/react";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

async function until(predicate: () => boolean): Promise<void> {
  for (let i = 0; i < 100; i++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("acquisition observation timed out");
}

function videoStream(): {
  stream: MediaStream;
  track: MediaStreamTrack;
  stops: () => number;
} {
  const track = document
    .createElement("canvas")
    .captureStream(1)
    .getVideoTracks()[0];
  const originalStop = track.stop.bind(track);
  let count = 0;
  track.stop = () => {
    count += 1;
    originalStop();
  };
  return { stream: new MediaStream([track]), track, stops: () => count };
}

export async function runAcquisitionContract() {
  const originalDescriptor = Object.getOwnPropertyDescriptor(
    navigator,
    "mediaDevices",
  );
  const userPending: ReturnType<typeof deferred<MediaStream>>[] = [];
  const displayPending: ReturnType<typeof deferred<MediaStream>>[] = [];
  let displayCalls = 0;
  let label = "";
  let listeners = 0;
  const devices = new EventTarget();
  const add = devices.addEventListener.bind(devices);
  const remove = devices.removeEventListener.bind(devices);
  devices.addEventListener = (...arguments_) => {
    listeners += 1;
    add(...arguments_);
  };
  devices.removeEventListener = (...arguments_) => {
    listeners -= 1;
    remove(...arguments_);
  };
  Object.assign(devices, {
    enumerateDevices: async () => [
      { kind: "videoinput", deviceId: "camera-id", label },
      { kind: "audioinput", deviceId: "mic-id", label: "Microphone" },
      { kind: "audiooutput", deviceId: "speaker-id", label: "Speaker" },
    ],
    getUserMedia: () => {
      const request = deferred<MediaStream>();
      userPending.push(request);
      return request.promise;
    },
    getDisplayMedia: () => {
      displayCalls += 1;
      const request = deferred<MediaStream>();
      displayPending.push(request);
      return request.promise;
    },
  });
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: devices,
  });

  let user!: CaptureResult;
  let display!: CaptureResult;
  let enumerated!: MediaDevicesResult;
  let failure: unknown;
  function Probe() {
    const [width, setWidth] = useState(640);
    const [displayAudio, setDisplayAudio] = useState(false);
    user = useUserMedia({ video: { width }, audio: false });
    display = useDisplayMedia({ video: true, audio: displayAudio });
    enumerated = useMediaDevices();
    return (
      <>
        <button
          id="acquire-user"
          onClick={() => {
            void user.request().catch((error) => {
              failure = error;
            });
          }}
        />
        <button
          id="acquire-display"
          onClick={() => {
            void display.request().catch((error) => {
              failure = error;
            });
          }}
        />
        <button id="stop-user" onClick={() => user.stop()} />
        <button
          id="next-width"
          onClick={() => setWidth((before) => before + 1)}
        />
        <button
          id="next-display"
          onClick={() => setDisplayAudio((before) => !before)}
        />
      </>
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
    await until(() => enumerated?.state === "ready");
    const initialLabels =
      enumerated.cameras[0]?.label === "" &&
      enumerated.microphones[0]?.id === "mic-id" &&
      enumerated.speakers[0]?.id === "speaker-id";
    label = "Camera";
    devices.dispatchEvent(new Event("devicechange"));
    await until(() => enumerated.cameras[0]?.label === "Camera");
    const captureDevices = initialLabels && listeners === 1;

    document.getElementById("acquire-user")!.click();
    await until(() => userPending.length === 1 && user.state === "requesting");
    document.getElementById("next-width")!.click();
    await until(() => userPending.length === 2);
    const obsolete = videoStream();
    userPending.shift()!.resolve(obsolete.stream);
    await until(() => obsolete.stops() === 1);
    const pendingReplacement =
      user.state === "requesting" && user.videoTrack === null;
    const selected = videoStream();
    userPending.shift()!.resolve(selected.stream);
    await until(() => user.state === "active" && user.videoTrack !== null);
    const capturePendingOptions = pendingReplacement && selected.stops() === 0;
    user.stop();
    await until(() => user.state === "idle" && selected.stops() === 1);

    document.getElementById("acquire-user")!.click();
    await until(() => userPending.length === 1 && user.state === "requesting");
    const first = videoStream();
    label = "Permitted Camera";
    userPending.shift()!.resolve(first.stream);
    await until(() => user.state === "active" && user.videoTrack !== null);
    await until(() => enumerated.cameras[0]?.label === "Permitted Camera");
    const firstSource = user.videoTrack;
    document.getElementById("next-width")!.click();
    await until(() => userPending.length === 1 && user.state === "requesting");
    const retainedWhilePending =
      user.videoTrack === firstSource && first.stops() === 0;
    userPending.shift()!.reject(new DOMException("busy", "NotReadableError"));
    await until(() => user.state === "active" && user.error !== null);
    const retainedAfterFailure =
      user.videoTrack === firstSource &&
      user.error?.code === "device-busy" &&
      first.stops() === 0;
    document.getElementById("next-width")!.click();
    await until(() => userPending.length === 1);
    const second = videoStream();
    userPending.shift()!.resolve(second.stream);
    await until(
      () => user.videoTrack !== firstSource && user.state === "active",
    );
    const captureReplacement =
      retainedWhilePending &&
      retainedAfterFailure &&
      user.videoTrack?.kind === "video" &&
      first.stops() === 1;

    document.getElementById("acquire-user")!.click();
    await until(() => userPending.length === 1);
    const stale = videoStream();
    document.getElementById("stop-user")!.click();
    await until(() => user.state === "idle");
    userPending.shift()!.resolve(stale.stream);
    await until(() => stale.stops() === 1 && failure !== undefined);
    const captureFencing =
      second.stops() === 1 &&
      user.videoTrack === null &&
      (failure as { code?: string })?.code === "request-cancelled";

    const cases = [
      ["NotAllowedError", "permission-denied"],
      ["NotFoundError", "device-not-found"],
      ["NotReadableError", "device-busy"],
      ["OverconstrainedError", "constraint-unsatisfied"],
      ["AbortError", "request-cancelled"],
      ["NotSupportedError", "not-supported"],
      ["UnexpectedError", "unknown"],
    ];
    let captureErrors = true;
    for (const [name, code] of cases) {
      failure = undefined;
      document.getElementById("acquire-user")!.click();
      await until(() => userPending.length === 1);
      userPending.shift()!.reject(new DOMException("capture failed", name));
      await until(() => failure !== undefined && user.state === "error");
      captureErrors &&=
        user.error?.code === code &&
        (failure as { code?: string })?.code === code;
    }

    const audioContext = new AudioContext();
    const voice = audioContext
      .createMediaStreamDestination()
      .stream.getAudioTracks()[0];
    const pairedVideo = videoStream();
    document.getElementById("acquire-user")!.click();
    await until(() => userPending.length === 1);
    userPending.shift()!.resolve(new MediaStream([pairedVideo.track, voice]));
    await until(() => user.state === "active" && user.audioTrack !== null);
    const bothKinds =
      user.videoTrack?.kind === "video" && user.audioTrack?.kind === "audio";
    voice.stop();
    voice.dispatchEvent(new Event("ended"));
    await until(() => user.audioTrack === null && user.state === "active");
    user.stop();
    await until(() => user.state === "idle");
    const captureSession =
      bothKinds && pairedVideo.stops() === 1 && voice.readyState === "ended";
    await audioContext.close();

    document.getElementById("next-display")!.click();
    await new Promise((resolve) => setTimeout(resolve, 30));
    const noPromptOnOptions = displayCalls === 0;
    document.getElementById("acquire-display")!.click();
    await until(() => displayPending.length === 1);
    displayPending
      .shift()!
      .reject(new DOMException("User cancelled", "NotAllowedError"));
    await until(() => display.state === "error");
    const cancelledDisplay = display.error?.code === "request-cancelled";
    document.getElementById("acquire-display")!.click();
    await until(() => displayPending.length === 1);
    const screen = videoStream();
    displayPending.shift()!.resolve(screen.stream);
    await until(() => display.state === "active");
    screen.track.stop();
    screen.track.dispatchEvent(new Event("ended"));
    await until(() => display.state === "idle");
    const captureDisplay =
      noPromptOnOptions &&
      cancelledDisplay &&
      displayCalls === 2 &&
      display.videoTrack === null;
    document.getElementById("acquire-user")!.click();
    await until(() => userPending.length === 1);
    const afterUnmount = videoStream();
    root.unmount();
    unmounted = true;
    userPending.shift()!.resolve(afterUnmount.stream);
    await until(() => afterUnmount.stops() === 1);
    return {
      captureDevices: captureDevices && listeners === 0,
      capturePendingOptions,
      captureReplacement,
      captureFencing: captureFencing && afterUnmount.stops() === 1,
      captureDisplay,
      captureErrors,
      captureSession,
    };
  } finally {
    if (!unmounted) root.unmount();
    host.remove();
    if (originalDescriptor)
      Object.defineProperty(navigator, "mediaDevices", originalDescriptor);
    else Reflect.deleteProperty(navigator, "mediaDevices");
  }
}
