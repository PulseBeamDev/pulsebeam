import assert from "node:assert/strict";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Agent, WatchStatus } from "@pulsebeam/web";
import {
  useWatch,
  type RoomRef,
  type Watch,
  type WatchConfig,
} from "../../src/index.js";
import { flush } from "./platform.js";
import { runtimes } from "./runtime.js";

export async function watchLifecycle() {
  const agent = new Agent({
    endpoint: "http://localhost",
    token: "watch-hook",
  });
  const runtime = runtimes.at(-1)!;
  const room = agent.room("pinned-room");
  const independent = await room
    .participant("sender")
    .data("events")
    .subscribe();
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  let current: Watch | null = null;
  let firstRender = true;
  function Probe({
    room,
    config,
  }: {
    room: RoomRef | null;
    config?: WatchConfig;
  }) {
    current = useWatch(room, config);
    if (firstRender) {
      assert.equal(
        current,
        null,
        "ownership is unavailable during initial render",
      );
      firstRender = false;
    }
    return (
      <output>{current?.state().page?.participants.length ?? "loading"}</output>
    );
  }
  const render = (room: RoomRef | null, config?: WatchConfig) =>
    flush(() =>
      root.render(
        <StrictMode>
          <Probe room={room} config={config} />
        </StrictMode>,
      ),
    );
  await render(null);
  assert.equal(runtime.catalogWatches.size, 0);
  await render(room);
  const initial = current!;
  assert.ok(initial);
  assert.equal(
    runtime.catalogWatches.size,
    1,
    "Strict Mode keeps one committed owner",
  );
  await render(room, { limit: 100, cursor: new Uint8Array() });
  assert.equal(
    current,
    initial,
    "equivalent config objects and bytes retain ownership",
  );
  const bytes = new Uint8Array(32);
  await render(room, { limit: 7, cursor: bytes });
  const second = current!;
  assert.notEqual(second, initial);
  assert.equal(initial.state().status, WatchStatus.Closed);
  bytes[0] = 9;
  assert.equal(
    second.state().cursor[0],
    0,
    "owned scope does not alias caller bytes",
  );
  await render(room, { limit: 7, cursor: new Uint8Array(32) });
  assert.equal(current, second);
  await render(room, { limit: 7, cursor: bytes });
  const third = current!;
  assert.notEqual(
    third,
    second,
    "changed cursor bytes replace the immutable scope",
  );
  assert.equal(second.state().status, WatchStatus.Closed);
  await render(room, { limit: 8, cursor: bytes });
  const fourth = current!;
  assert.notEqual(fourth, third, "changed limit replaces ownership");
  assert.equal(third.state().status, WatchStatus.Closed);
  const [owner] = [...runtime.catalogWatches].find(
    ([, scope]) => !scope.closed,
  )!;
  await flush(() =>
    runtime.setObservation(owner, {
      ...fourth.state(),
      observationRevision: 2n,
      status: WatchStatus.Live,
      page: Object.freeze({
        participants: Object.freeze([
          {
            id: "presence",
            publications: Object.freeze([]),
          },
        ]),
      }),
    }),
  );
  assert.equal(
    host.textContent,
    "1",
    "watch updates rerender the real hook consumer",
  );
  await render(agent.room("other-room"));
  const other = current!;
  assert.equal(fourth.state().status, WatchStatus.Closed);
  assert.equal(other.state().room, "other-room");
  await render(null);
  assert.equal(current, null);
  assert.equal(other.state().status, WatchStatus.Closed);
  await render(room);
  const last = current!;
  await flush(() => root.unmount());
  assert.equal(last.state().status, WatchStatus.Closed);
  assert.equal(
    [...runtime.catalogWatches.values()].filter((scope) => !scope.closed)
      .length,
    0,
  );
  assert.equal(
    independent.isClosed(),
    false,
    "watch replacement and unmount do not close independent subscriptions",
  );
  await independent.close();
  await agent.close();
  host.remove();
}
