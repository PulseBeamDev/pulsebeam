import assert from "node:assert/strict";
import { startTransition, Suspense, useLayoutEffect } from "react";
import { createRoot } from "react-dom/client";
import { useUserMedia, Video, type CaptureResult } from "../../src/index.js";
import { createCaptureSource } from "@pulsebeam/web";
import { captures, deferred, flush, stream } from "./platform.js";

export async function concurrentRender() {
  const preview = stream();
  const acquired = stream();
  const playback = deferred<void>();
  const never = new Promise<void>(() => {});
  const source = createCaptureSource(
    preview.getVideoTracks()[0] as unknown as MediaStreamTrack,
    "video",
  );
  let current: CaptureResult;
  let suspended = false;
  let committedFailures = 0;
  let abandonedFailures = 0;
  const originalPlay = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = () => playback.promise;
  function Suspend(): never {
    suspended = true;
    throw never;
  }
  function Probe({ speculative }: { speculative: boolean }) {
    const capture = useUserMedia({
      video: { width: speculative ? 1280 : 640 },
      audio: false,
    });
    useLayoutEffect(() => {
      current = capture;
    }, [capture]);
    return (
      <>
        <Video
          source={source}
          onPlaybackError={() => {
            if (speculative) abandonedFailures++;
            else committedFailures++;
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
    await flush(() =>
      root.render(
        <Suspense fallback={null}>
          <Probe speculative={false} />
        </Suspense>,
      ),
    );
    let request: Promise<void>;
    await flush(() => {
      request = current!.request();
    });
    const pending = captures.at(-1)!;
    await flush(() =>
      startTransition(() =>
        root.render(
          <Suspense fallback={null}>
            <Probe speculative />
          </Suspense>,
        ),
      ),
    );
    assert.equal(
      suspended,
      true,
      "speculative render actually reached suspension",
    );
    await flush(() => {
      pending.resolve(acquired);
      playback.reject(new DOMException("blocked", "NotAllowedError"));
    });
    await request!;
    assert.equal(
      current!.state,
      "active",
      "abandoned options do not cancel committed capture",
    );
    assert.equal(committedFailures, 1);
    assert.equal(
      abandonedFailures,
      0,
      "abandoned callback never handles committed playback",
    );
  } finally {
    await flush(() => root.unmount());
    preview.getTracks().forEach((track) => track.stop());
    assert.ok(
      acquired.getTracks().every((track) => track.readyState === "ended"),
    );
    HTMLMediaElement.prototype.play = originalPlay;
    host.remove();
  }
}
