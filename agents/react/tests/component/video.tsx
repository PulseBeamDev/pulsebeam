import assert from "node:assert/strict";
import { useState } from "react";
import { createRoot } from "react-dom/client";
import {
  useAgent,
  Video,
  type Agent,
  type VideoPreferences,
} from "../../src/index.js";
import { flush } from "./platform.js";
import { runtimes } from "./runtime.js";

export async function videoPreferences() {
  let agent!: Agent;
  let change!: (
    patch: Partial<{
      first: VideoPreferences;
      second: VideoPreferences;
      hide: boolean;
      remove: boolean;
      reverse: boolean;
    }>,
  ) => void;
  const original = HTMLVideoElement.prototype.getBoundingClientRect;
  HTMLVideoElement.prototype.getBoundingClientRect = function () {
    const height = this.dataset.size === "large" ? 240 : 120;
    return {
      top: 0,
      left: 0,
      right: 320,
      bottom: height,
      width: 320,
      height,
      x: 0,
      y: 0,
      toJSON() {
        return {};
      },
    };
  };
  function Room() {
    agent = useAgent({ endpoint: "http://localhost", token: "token" });
    const [state, setState] = useState({
      first: {
        minHeight: 360,
        minFps: 15,
        priority: 1,
        latency: { minMs: 100, maxMs: 200 },
      } as VideoPreferences,
      second: {
        minHeight: 720,
        minFps: 30,
        priority: 5,
        latency: { minMs: 0, maxMs: 50 },
      } as VideoPreferences,
      hide: false,
      remove: false,
      reverse: false,
    });
    change = (patch) => setState((before) => ({ ...before, ...patch }));
    const remote = agent.remote.participant("alice").video("camera");
    const views = [
      <Video
        key="one"
        source={remote}
        preferences={state.first}
        data-size="small"
      />,
      !state.remove && (
        <Video
          key="two"
          source={remote}
          preferences={state.second}
          hidden={state.hide}
          data-size="large"
        />
      ),
    ];
    return <>{state.reverse ? views.reverse() : views}</>;
  }
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  try {
    await flush(() => root.render(<Room />));
    const runtime = runtimes.at(-1)!;
    const remote = agent.remote.participant("alice").video("camera");
    assert.equal(
      "setReceiveOptions" in remote,
      false,
      "normal runtime handle has no competing imperative setter",
    );
    assert.equal(
      "agent" in agent.remote,
      false,
      "remote facade cannot expose the managed Agent",
    );
    assert.equal("advanced" in agent, false);
    assert.equal("getSnapshot" in agent, false);
    assert.equal("close" in agent, false);
    assert.deepEqual(
      runtime.desired.video,
      [],
      "absent remote source creates no demand",
    );
    await flush(() =>
      runtime.emit({
        participants: [{ id: "alice-id", externalId: "alice" }],
        publications: [
          {
            id: "camera-id",
            participantId: "alice-id",
            kind: "video",
            label: "camera",
          },
        ],
      }),
    );
    const desired = () => runtime.desired.video[0];
    assert.deepEqual(desired(), {
      slot: 0,
      trackId: "camera-id",
      height: 240,
      minHeight: 720,
      minFps: 30,
      priority: 5,
      playoutDelay: { mode: "fixed", minMs: 0, maxMs: 200 },
    });
    const aggregate = { ...desired() };
    await flush(() => change({ reverse: true }));
    assert.deepEqual(desired(), aggregate, "aggregation is order independent");
    await flush(() => change({ hide: true }));
    await flush(() => window.dispatchEvent(new Event("resize")));
    assert.deepEqual(desired(), {
      slot: 0,
      trackId: "camera-id",
      height: 120,
      minHeight: 360,
      minFps: 15,
      priority: 1,
      playoutDelay: { mode: "fixed", minMs: 100, maxMs: 200 },
    });
    await flush(() => change({ first: { latency: { minMs: 0, maxMs: 0 } } }));
    assert.deepEqual(desired().playoutDelay, {
      mode: "fixed",
      minMs: 0,
      maxMs: 0,
    });
    await flush(() => change({ first: {} }));
    assert.equal(
      "playoutDelay" in desired(),
      false,
      "withdrawn latency requests defaults, not zero bounds",
    );
    await flush(() => change({ hide: false, remove: true }));
    assert.equal(desired().height, 120);
    assert.equal("playoutDelay" in desired(), false);
    host.remove();
    await flush(() => window.dispatchEvent(new Event("resize")));
    assert.deepEqual(
      runtime.desired.video,
      [],
      "detached presentation contributes no demand",
    );
    assert.equal(agent.remote.participant("alice").video("camera"), remote);
  } finally {
    await flush(() => root.unmount());
    HTMLVideoElement.prototype.getBoundingClientRect = original;
    host.remove();
  }
}
