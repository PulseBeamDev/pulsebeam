import { startTransition, Suspense, useLayoutEffect } from "react";
import { createRoot } from "react-dom/client";
import { useUserMedia, Video, type CaptureResult } from "@pulsebeam/react";
import { createCaptureSource } from "@pulsebeam/web";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

async function until(predicate: () => boolean) {
  for (let i = 0; i < 100; i++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("concurrent render observation timed out");
}

export async function runConcurrencyContract() {
  const descriptor = Object.getOwnPropertyDescriptor(navigator, "mediaDevices");
  const originalPlay = HTMLMediaElement.prototype.play;
  const capture = deferred<MediaStream>();
  const playback = deferred<void>();
  const never = new Promise<void>(() => {});
  const preview = document.createElement("canvas").captureStream(1);
  const acquired = document.createElement("canvas").captureStream(1);
  const source = createCaptureSource(preview.getVideoTracks()[0], "video");
  let current: CaptureResult | undefined;
  let suspended = false;
  let playCalls = 0;
  let committedFailures = 0;
  let abandonedFailures = 0;
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: { getUserMedia: () => capture.promise },
  });
  HTMLMediaElement.prototype.play = function () {
    playCalls += 1;
    return playback.promise;
  };
  function Suspend(): never {
    suspended = true;
    throw never;
  }
  function Probe({ speculative }: { speculative: boolean }) {
    const user = useUserMedia({
      video: { width: speculative ? 1280 : 640 },
      audio: false,
    });
    useLayoutEffect(() => {
      current = user;
    }, [user]);
    return (
      <>
        <Video
          source={source}
          onPlaybackError={() => {
            if (speculative) abandonedFailures += 1;
            else committedFailures += 1;
          }}
        />
        {speculative && <Suspend />}
      </>
    );
  }
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  try {
    root.render(
      <Suspense fallback={null}>
        <Probe speculative={false} />
      </Suspense>,
    );
    await until(() => current !== undefined && playCalls === 1);
    const request = current!.request().then(
      () => true,
      () => false,
    );
    await until(() => current?.state === "requesting");
    startTransition(() => {
      root.render(
        <Suspense fallback={null}>
          <Probe speculative />
        </Suspense>,
      );
    });
    await until(() => suspended);
    capture.resolve(acquired);
    playback.reject(new DOMException("blocked", "NotAllowedError"));
    const accepted = await request;
    await until(() => committedFailures + abandonedFailures > 0);
    return {
      committedRenderIsolation:
        accepted && committedFailures === 1 && abandonedFailures === 0,
    };
  } finally {
    root.unmount();
    host.remove();
    for (const track of [...preview.getTracks(), ...acquired.getTracks()])
      track.stop();
    HTMLMediaElement.prototype.play = originalPlay;
    if (descriptor)
      Object.defineProperty(navigator, "mediaDevices", descriptor);
    else Reflect.deleteProperty(navigator, "mediaDevices");
  }
}
