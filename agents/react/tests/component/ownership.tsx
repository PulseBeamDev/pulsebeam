import assert from "node:assert/strict";
import { Component, StrictMode, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import {
  Audio,
  Data,
  Publish,
  track,
  useAgent,
  type Agent,
  type DataEvent,
} from "../../src/index.js";
import { createCaptureSource, ResourceOwnershipError } from "@pulsebeam/web";
import { publishableVideoSource } from "../../src/sources.js";
import { flush, Track } from "./platform.js";
import { runtimes } from "./runtime.js";

class Boundary extends Component<
  { children: ReactNode; onError(error: Error): void },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(error: Error) {
    this.props.onError(error);
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}

export async function ownershipAdapters() {
  let agent!: Agent;
  let update!: (
    patch: Partial<{ source: boolean; enabled: boolean; duplicate: boolean }>,
  ) => void;
  let duplicateFailure: Error | undefined;
  const events: DataEvent<"ordered">[] = [];
  const native = new Track("video");
  const source = publishableVideoSource(
    { kind: "video" },
    createCaptureSource(native as unknown as MediaStreamTrack, "video"),
  );

  function Room() {
    agent = useAgent({ endpoint: "http://localhost", token: "ownership" });
    const [state, setState] = useState({
      source: true,
      enabled: true,
      duplicate: false,
    });
    update = (patch) => setState((before) => ({ ...before, ...patch }));
    const camera = agent.track(track.video("camera"));
    const messages = agent.track(track.data("messages", "ordered"));
    return (
      <>
        <Audio agent={agent} />
        <Data track={messages} onEvent={(event) => events.push(event)} />
        <Publish
          track={camera}
          source={state.source ? source : null}
          enabled={state.enabled}
          content="text"
        />
        {state.duplicate && (
          <Boundary onError={(error) => (duplicateFailure = error)}>
            <Publish track={camera} source={source} />
          </Boundary>
        )}
      </>
    );
  }

  const host = document.createElement("div");
  const root = createRoot(host);
  await flush(() =>
    root.render(
      <StrictMode>
        <Room />
      </StrictMode>,
    ),
  );
  const runtime = runtimes.at(-1)!;
  assert.equal(runtime.claimedPublications.size, 1);
  assert.notEqual(runtime.audioOwner, undefined);
  assert.equal(runtime.dataOwners.size, 1);
  runtime.deliverData("messages", {
    type: "message",
    participantId: "alice",
    trackId: "message-id",
    sequence: 1n,
    data: new Uint8Array([0, 255]),
  });
  await flush();
  assert.equal(events.length, 1);
  assert.deepEqual(events[0]?.data, new Uint8Array([0, 255]));
  assert.equal(runtime.ownedReplacements.at(-1)?.track, native);
  assert.equal(runtime.ownedReplacements.at(-1)?.config.contentHint, "text");

  await flush(() => update({ enabled: false }));
  assert.ok([...runtime.ownedMuted.values()].includes(true));
  await flush(() => update({ source: false }));
  assert.equal(runtime.ownedReplacements.at(-1)?.track, null);
  assert.equal(native.readyState, "live");

  await flush(() => update({ source: true, duplicate: true }));
  await flush();
  assert.ok(duplicateFailure instanceof ResourceOwnershipError);
  assert.equal(runtime.claimedPublications.size, 1);

  await flush(() => root.unmount());
  assert.equal(runtime.claimedPublications.size, 0);
  assert.equal(runtime.audioOwner, undefined);
  assert.equal(runtime.dataOwners.size, 0);
  assert.equal(native.readyState, "live");
}
