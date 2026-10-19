import assert from "node:assert/strict";
import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Video,
  useAgent,
  useUserMedia,
  useMediaDevices,
} from "../../src/index.js";
import { captures, deferred, flush, stream } from "./platform.js";
import { runtimes } from "./runtime.js";
import { concurrentRender } from "./concurrency.js";

await concurrentRender();

let agent: ReturnType<typeof useAgent>;
let capture: ReturnType<typeof useUserMedia>;
let updateToken: () => void;
let replaceSession: () => void;
let playback: { retry(): Promise<void> } | undefined;
let devicesReady = false;
function Room() {
  const [token, setToken] = useState("first");
  const [endpoint, setEndpoint] = useState("http://localhost");
  agent = useAgent({ endpoint, token, topology: { localVideos: 1 } });
  capture = useUserMedia({ video: true, audio: true });
  devicesReady = useMediaDevices().state === "ready";
  updateToken = () => setToken("renewed");
  replaceSession = () => setEndpoint("http://localhost/replacement");
  const camera = agent.local.video("camera");
  useEffect(() => {
    camera.setSource(capture.videoTrack);
    return () => camera.setSource(null);
  }, [camera, capture.videoTrack]);
  return (
    <section>
      <output>{agent.getSnapshot().desiredRevision}</output>
      <Video
        source={camera}
        mirror
        onPlaybackError={(failure) => {
          playback = failure;
        }}
      />
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
assert.equal(element.muted, true);
assert.match(element.style.transform, /scaleX\(-1\)/);
const first = agent!;
const runtime = runtimes.at(-1)!;
await flush(() => runtime.emit({ desiredRevision: 17 }));
assert.equal(
  host.querySelector("output")!.textContent,
  "17",
  "snapshot subscription renders",
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
  agent!.local.video("camera").setEnabled(false);
});
assert.equal(
  element.srcObject,
  null,
  "muting detaches preview without stopping capture",
);
assert.equal(owned.getVideoTracks()[0].readyState, "live");
await flush(() => {
  agent!.local.video("camera").setEnabled(true);
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
assert.equal(first.getSnapshot().connection, "disconnected");

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
await flush(() => captures.at(-1)!.resolve(late));
assert.equal((await cancelled!).code, "request-cancelled");
assert.ok(
  [...owned.getTracks(), ...fresh.getTracks(), ...late.getTracks()].every(
    (track) => track.readyState === "ended",
  ),
  "unmount releases current and stale captures, including muted audio",
);
assert.equal(element.srcObject, null);
assert.equal(host.childElementCount, 0);
assert.ok(runtimes.every((runtime) => runtime.closed));
host.remove();
console.log(
  "React component lifecycle, capture cancellation, ownership and playback retry passed",
);
