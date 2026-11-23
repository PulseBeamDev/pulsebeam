import assert from "node:assert/strict";
import { startTransition, Suspense, useLayoutEffect } from "react";
import { createRoot } from "react-dom/client";
import {
  Audio,
  Data,
  Video,
  useAgent,
  usePublish,
  useSubscribe,
} from "../../src/index.js";
import { useCapture, type CaptureResult } from "../../src/index.js";
import {
  LocalVideoTrack,
  RemoteVideoTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  VideoSource,
} from "@pulsebeam/web";
import { publishableVideoSource } from "../../src/sources.js";
import { captures, deferred, flush, stream } from "./platform.js";

export async function concurrentRender() {
  const preview = stream();
  const acquired = stream();
  const publish = LocalVideoTrack.prototype.publish;
  const videoSubscribe = RemoteVideoTrack.prototype.subscribe;
  const audioSubscribe = RemoteAudioTrack.prototype.subscribe;
  const dataSubscribe = RemoteDataTrack.prototype.subscribe;
  let claims = 0;
  LocalVideoTrack.prototype.publish = function (...args) {
    claims++;
    return publish.call(this, ...args);
  };
  RemoteVideoTrack.prototype.subscribe = function (...args) {
    claims++;
    return videoSubscribe.call(this, ...args);
  };
  RemoteAudioTrack.prototype.subscribe = function (...args) {
    claims++;
    return audioSubscribe.call(this, ...args);
  };
  RemoteDataTrack.prototype.subscribe = function (...args) {
    claims++;
    return dataSubscribe.call(this, ...args);
  };
  const playback = deferred<void>();
  const never = new Promise<void>(() => {});
  const input = VideoSource.fromTrack(
    preview.getVideoTracks()[0] as unknown as MediaStreamTrack,
  );
  const source = publishableVideoSource({ kind: "video" }, input);
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
    const agent = useAgent({
      endpoint: "http://localhost",
      token: "abandoned",
    });
    return agent ? (
      <CommittedProbe agent={agent} speculative={speculative} />
    ) : null;
  }
  function CommittedProbe({
    agent,
    speculative,
  }: {
    agent: NonNullable<ReturnType<typeof useAgent>>;
    speculative: boolean;
  }) {
    usePublish(speculative ? agent.video("camera") : null, source);
    const messages = agent
      .room("selected-room")
      .participant("alice")
      .data("messages");
    const remote = agent
      .room("selected-room")
      .participant("alice")
      .video("camera");
    const video = useSubscribe(speculative ? remote : null);
    const audio = useSubscribe(
      speculative
        ? agent.room("selected-room").participant("alice").audio("microphone")
        : null,
    );
    const capture = useCapture("device", {
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
        {speculative && (
          <>
            <Audio source={audio} />
            <Data
              track={messages}
              onEvent={() => {
                abandonedFailures++;
              }}
            />
            <Video source={video} />
            <Suspend />
          </>
        )}
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
    assert.equal(claims, 0, "lookup and abandoned owners acquire no resources");
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
    await flush();
    acquired.getTracks().forEach((track) => track.stop());
    assert.ok(
      acquired.getTracks().every((track) => track.readyState === "ended"),
    );
    await input.close();
    LocalVideoTrack.prototype.publish = publish;
    RemoteVideoTrack.prototype.subscribe = videoSubscribe;
    RemoteAudioTrack.prototype.subscribe = audioSubscribe;
    RemoteDataTrack.prototype.subscribe = dataSubscribe;
    HTMLMediaElement.prototype.play = originalPlay;
    host.remove();
  }
}
