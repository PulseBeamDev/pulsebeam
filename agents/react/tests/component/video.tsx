import assert from "node:assert/strict";
import { createRoot } from "react-dom/client";
import {
  AudioSource,
  VideoSource,
  type AudioSubscription,
  type VideoSubscription,
  type WebAudioOutput,
  type WebVideoOutput,
} from "@pulsebeam/web";
import { Audio, Video, useAgent, type Agent } from "../../src/index.js";
import { deferred, flush, Track } from "./platform.js";

// This entry point is shared by the component lifecycle runner.
export async function playbackLifetimes() {
  let agent!: Agent;
  function Room() {
    const current = useAgent({
      endpoint: "http://localhost",
      token: "playback",
    });
    if (current) agent = current;
    return null;
  }
  const ownerRoot = createRoot(document.createElement("div"));
  await flush(() => ownerRoot.render(<Room />));
  const remote = agent.room("room").participant("alice");
  const video = remote.video("camera").subscribe();
  const audio = remote.audio("microphone").subscribe();
  await flush();

  // Control only the physical observation boundary, not Core admission policy.
  const requests: {
    signal?: AbortSignal;
    result: ReturnType<typeof deferred<WebAudioOutput | WebVideoOutput>>;
  }[] = [];
  function intercept(subscription: AudioSubscription | VideoSubscription) {
    subscription.playbackOutput = ((options?: { signal: AbortSignal }) => {
      const result = deferred<WebAudioOutput | WebVideoOutput>();
      requests.push({ signal: options?.signal, result });
      return result.promise;
    }) as typeof subscription.playbackOutput;
  }
  intercept(video);
  intercept(audio);
  function complete(
    request: (typeof requests)[number],
    kind: "video" | "audio",
  ) {
    const track = new Track(kind);
    let closed = false;
    let closes = 0;
    request.result.resolve({
      track: () =>
        closed ? undefined : (track as unknown as MediaStreamTrack),
      close: async () => {
        closed = true;
        closes++;
      },
    } as WebAudioOutput);
    return {
      track,
      get closes() {
        return closes;
      },
    };
  }
  const host = document.createElement("div");
  const root = createRoot(host);
  const play = HTMLMediaElement.prototype.play;
  try {
    for (const kind of ["video", "audio"] as const) {
      const subscription = kind === "video" ? video : audio;
      const render = (count: number) =>
        kind === "video" ? (
          <>
            {count > 0 && (
              <Video
                source={video}
                data-testid="one"
                muted
                autoPlay
                playsInline
              />
            )}
            {count > 1 && <Video source={video} data-testid="two" />}
          </>
        ) : (
          <>
            {count > 0 && (
              <Audio source={audio} data-testid="one" muted autoPlay />
            )}
            {count > 1 && <Audio source={audio} data-testid="two" />}
          </>
        );
      const start = requests.length;
      await flush(() => root.render(render(2)));
      assert.equal(
        requests.length - start,
        2,
        `${kind}: each view borrows an observation`,
      );
      const first = complete(requests[start]!, kind);
      const second = complete(requests[start + 1]!, kind);
      await flush();
      const one = host.querySelector<HTMLMediaElement>('[data-testid="one"]')!;
      const two = host.querySelector<HTMLMediaElement>('[data-testid="two"]')!;
      assert.ok(
        (one.srcObject as MediaStream)
          .getTracks()
          .includes(first.track as unknown as MediaStreamTrack),
      );
      assert.ok(
        (two.srcObject as MediaStream)
          .getTracks()
          .includes(second.track as unknown as MediaStreamTrack),
      );
      assert.equal(one.muted, true);
      assert.equal(one.hasAttribute("autoplay"), true);
      if (kind === "video") assert.equal(one.hasAttribute("playsinline"), true);
      await flush(() => root.render(render(1)));
      assert.equal(first.closes, 0, `${kind}: surviving view keeps its clone`);
      assert.equal(second.closes, 1, `${kind}: removed view closes its clone`);
      await flush(() => root.render(null));
      assert.equal(first.closes, 1);
      assert.equal(
        subscription.state().closed,
        false,
        `${kind}: caller retains subscription`,
      );
      const pending = requests.length;
      await flush(() => root.render(render(1)));
      assert.equal(requests.length, pending + 1);
      await flush(() => root.render(null));
      assert.equal(
        requests[pending]!.signal?.aborted,
        true,
        `${kind}: unmount aborts pending acquisition`,
      );
      const late = complete(requests[pending]!, kind);
      await flush();
      assert.equal(late.closes, 1, `${kind}: late clone is closed`);
      assert.equal(subscription.state().closed, false);

      const replacement =
        kind === "video"
          ? remote.video("other-camera").subscribe()
          : remote.audio("other-microphone").subscribe();
      intercept(replacement);
      await flush();
      const renderSource = (next: boolean) =>
        kind === "video" ? (
          <Video source={next ? (replacement as typeof video) : video} />
        ) : (
          <Audio source={next ? (replacement as typeof audio) : audio} />
        );
      const replaced = requests.length;
      await flush(() => root.render(renderSource(false)));
      const element = host.querySelector<HTMLMediaElement>(kind)!;
      await flush(() => root.render(renderSource(true)));
      assert.equal(requests.length, replaced + 2);
      assert.equal(
        requests[replaced]!.signal?.aborted,
        true,
        `${kind}: replacement aborts the old acquisition`,
      );
      const current = complete(requests[replaced + 1]!, kind);
      await flush();
      const stale = complete(requests[replaced]!, kind);
      await flush();
      assert.equal(
        stale.closes,
        1,
        `${kind}: stale completion closes its clone`,
      );
      assert.ok(
        (element.srcObject as MediaStream)
          .getTracks()
          .includes(current.track as unknown as MediaStreamTrack),
      );
      assert.ok(
        !(element.srcObject as MediaStream)
          .getTracks()
          .includes(stale.track as unknown as MediaStreamTrack),
        `${kind}: stale completion cannot reattach`,
      );
      await flush(() => root.render(null));
      assert.equal(current.closes, 1);
      assert.equal(replacement.state().closed, false);
      await replacement.close();
    }

    const capturedVideo = new Track("video");
    const capturedAudio = new Track("audio");
    const videoSource = VideoSource.fromTrack(
      capturedVideo as unknown as MediaStreamTrack,
    );
    const audioSource = AudioSource.fromTrack(
      capturedAudio as unknown as MediaStreamTrack,
    );
    const blocked = deferred<void>();
    const error = new Error("autoplay blocked");
    let calls = 0;
    let oldCallback = 0;
    let failure: { error: unknown; retry(): Promise<void> } | undefined;
    HTMLMediaElement.prototype.play = () => {
      calls++;
      return blocked.promise;
    };
    await flush(() =>
      root.render(
        <Video source={videoSource} onPlaybackError={() => oldCallback++} />,
      ),
    );
    await flush(() =>
      root.render(
        <Video
          source={videoSource}
          onPlaybackError={(value) => {
            failure = value;
          }}
        />,
      ),
    );
    await flush(() => blocked.reject(error));
    assert.equal(
      oldCallback,
      0,
      "the latest committed playback callback is used",
    );
    assert.equal(failure?.error, error);
    const retry = failure!.retry;
    await flush(() =>
      root.render(<Audio source={audioSource} autoPlay={false} />),
    );
    const audioElement = host.querySelector("audio")!;
    assert.ok(
      (audioElement.srcObject as MediaStream)
        .getTracks()
        .includes(capturedAudio as unknown as MediaStreamTrack),
    );
    const beforeRetry = calls;
    await flush(() => retry());
    assert.equal(calls, beforeRetry, "retry from a retired source is inert");
    const stalePlay = deferred<void>();
    let staleCallbacks = 0;
    HTMLMediaElement.prototype.play = () => stalePlay.promise;
    await flush(() =>
      root.render(
        <Video source={videoSource} onPlaybackError={() => staleCallbacks++} />,
      ),
    );
    await flush(() =>
      root.render(<Audio source={audioSource} autoPlay={false} />),
    );
    await flush(() => stalePlay.reject(new Error("late playback failure")));
    assert.equal(
      staleCallbacks,
      0,
      "retired playback cannot report a late failure",
    );
    await flush(() => root.render(null));
    assert.equal(capturedVideo.stopped, 0);
    assert.equal(capturedAudio.stopped, 0);
    assert.equal(videoSource.previewTrack(), capturedVideo);
    assert.equal(audioSource.previewTrack(), capturedAudio);
    await flush(() =>
      root.render(<Video source={videoSource} autoPlay={false} />),
    );
    const preview = host.querySelector("video")!;
    assert.notEqual(preview.srcObject, null);
    await flush(() => videoSource.close());
    assert.equal(
      preview.srcObject,
      null,
      "closing a source withdraws its preview",
    );
    assert.equal(
      capturedVideo.readyState,
      "live",
      "the supplied browser track remains caller-owned",
    );
    await audioSource.close();
  } finally {
    HTMLMediaElement.prototype.play = play;
    await flush(() => root.unmount());
    for (const request of requests)
      request.result.resolve({
        track: () => undefined,
        close: async () => {},
      } as WebAudioOutput);
    await flush();
    await video.close();
    await audio.close();
    await flush(() => ownerRoot.unmount());
  }
}
