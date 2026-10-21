import assert from "node:assert/strict";
import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { Publish, Video, track, useAgent } from "../../src/index.js";
import { useUserMedia, useMediaDevices } from "../../src/capture.js";
import { captures, deferred, flush, stream } from "./platform.js";
import { runtimes } from "./runtime.js";
import { concurrentRender } from "./concurrency.js";

import { captureLifecycle } from "./capture.js";
import { ownershipAdapters } from "./ownership.js";
import { videoPreferences } from "./video.js";

await concurrentRender();
await captureLifecycle();
await ownershipAdapters();
await videoPreferences();

let agent: ReturnType<typeof useAgent>;
let capture: ReturnType<typeof useUserMedia>;
let updateToken: () => void;
let replaceSession: () => void;
let playback: { retry(): Promise<void> } | undefined;
let devicesReady = false;
let setEnabled!: (enabled: boolean) => void;
function Room() {
  const [token, setToken] = useState("first");
  const [endpoint, setEndpoint] = useState("http://localhost");
  agent = useAgent({ endpoint, token, topology: { localVideos: 1 } });
  capture = useUserMedia({ video: true, audio: true });
  devicesReady = useMediaDevices().state === "ready";
  updateToken = () => setToken("renewed");
  replaceSession = () => setEndpoint("http://localhost/replacement");
  const camera = agent.track(track.video("camera"));
  const [enabled, updateEnabled] = useState(true);
  setEnabled = updateEnabled;
  return (
    <section>
      <Publish track={camera} source={capture.video} enabled={enabled} />
      <output>{agent.connection}</output>
      <Video
        source={enabled ? capture.video : null}
        mirror
        onPlaybackError={(failure) => {
          playback = failure;
        }}
      />
      <Video source={capture.video} data-testid="captured-preview" />
    </section>
  );
}
const host = document.createElement("div");
document.body.append(host);
const root = createRoot(host);
await flush(() =>
  root.render(
    <StrictMode>
      <Room />
    </StrictMode>,
  ),
);
assert.equal(devicesReady, true);
assert.equal(
  runtimes.filter((runtime) => !runtime.closed).length,
  1,
  "StrictMode has one live runtime",
);
let request: Promise<void>;
await flush(() => {
  request = capture.request();
});
const owned = stream();
await flush(() => captures.at(-1)!.resolve(owned));
await request!;
assert.equal(capture.state, "active");
const element = host.querySelector("video")!;
assert.equal(
  (element.srcObject as any).getVideoTracks()[0],
  owned.getVideoTracks()[0],
);
const capturedPreview = host.querySelector<HTMLVideoElement>(
  '[data-testid="captured-preview"]',
)!;
assert.equal(
  (capturedPreview.srcObject as any).getVideoTracks()[0],
  owned.getVideoTracks()[0],
  "grouped capture sources render directly through the shared source contract",
);
assert.deepEqual(
  runtimes.at(-1)!.desired.video,
  [],
  "local and captured previews create no remote demand",
);
assert.equal(element.muted, true);
assert.match(element.style.transform, /scaleX\(-1\)/);
const first = agent!;
const runtime = runtimes.at(-1)!;
await flush(() => runtime.emit({ connection: "connected" }));
assert.equal(
  host.querySelector("output")!.textContent,
  "connected",
  "reactive state renders",
);
await flush(updateToken!);
assert.equal(agent!, first, "token renewal does not replace session");
assert.equal(runtime.config.token, "renewed");

let plays = 0;
HTMLMediaElement.prototype.play = async () => {
  plays++;
  throw new DOMException("blocked", "NotAllowedError");
};
await flush(() => {
  setEnabled(false);
});
assert.equal(
  element.srcObject,
  null,
  "muting detaches preview without stopping capture",
);
assert.equal(owned.getVideoTracks()[0].readyState, "live");
assert.equal(
  (capturedPreview.srcObject as any).getVideoTracks()[0],
  owned.getVideoTracks()[0],
  "publication mute cannot detach a captured-source preview",
);
await flush(() => {
  setEnabled(true);
});
assert.ok(playback, "playback rejection reaches production callback");
HTMLMediaElement.prototype.play = async () => {
  plays++;
};
await flush(() => playback!.retry());
assert.equal(plays, 2, "retry reaches the attached element");

const completion = deferred<void>();
runtime.replaceCompletion = completion.promise;
await flush(replaceSession!);
assert.notEqual(agent!, first);
assert.equal(
  runtime.closed,
  true,
  "session replacement closes retired runtime",
);
assert.equal(
  owned.getVideoTracks()[0].readyState,
  "live",
  "runtime does not own caller capture",
);
await flush(() => completion.resolve());
assert.equal(first.connection, "disconnected");

let older: Promise<unknown>;
let newer: Promise<void>;
await flush(() => {
  older = capture.request().catch((error) => error);
});
const olderCompletion = captures.at(-1)!;
await flush(() => {
  newer = capture.request();
});
const fresh = stream();
await flush(() => captures.at(-1)!.resolve(fresh));
await newer!;
const superseded = stream();
await flush(() => olderCompletion.resolve(superseded));
assert.equal((await older!).code, "request-cancelled");
assert.ok(
  superseded.getTracks().every((track) => track.readyState === "ended"),
);
assert.ok(
  owned.getTracks().every((track) => track.readyState === "ended"),
  "replacement releases previous capture",
);
assert.equal(capture.state, "active");
let cancelled: Promise<unknown>;
await flush(() => {
  cancelled = capture.request().catch((error) => error);
});
const late = stream();
await flush(() => root.unmount());
assert.equal(
  (await cancelled!).code,
  "request-cancelled",
  "unmount settles pending actions before the platform completes",
);
await flush(() => captures.at(-1)!.resolve(late));
assert.ok(
  [...owned.getTracks(), ...fresh.getTracks(), ...late.getTracks()].every(
    (track) => track.readyState === "ended",
  ),
  "unmount releases current and stale captures, including muted audio",
);
assert.equal(element.srcObject, null);
assert.equal(capturedPreview.srcObject, null);
assert.equal(host.childElementCount, 0);
assert.ok(runtimes.every((runtime) => runtime.closed));
host.remove();
console.log(
  "React component lifecycle, capture cancellation, ownership and playback retry passed",
);
