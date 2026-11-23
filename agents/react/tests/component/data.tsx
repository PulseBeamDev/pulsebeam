import assert from "node:assert/strict";
import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { Data, useAgent, type Agent, DataEvent } from "../../src/index.js";
import { deferred, flush } from "./platform.js";
import { captureDataReads } from "./scripted.js";

const message = (sequence: bigint) =>
  DataEvent.Message.new({
    participantId: "alice",
    publicationId: "chat-id",
    sequence,
    data: Uint8Array.of(Number(sequence)),
  });

export async function dataLifetimes() {
  let agent!: Agent;
  let update!: (
    patch: Partial<{
      version: number;
      trackId: string;
      mounted: boolean;
      generation: number;
    }>,
  ) => void;
  const inbox = captureDataReads();
  const pending = deferred<void>();
  const retired = deferred<void>();
  const calls: string[] = [];
  const other: bigint[] = [];
  const errors: string[] = [];
  let fail = false;
  function Room() {
    const current = useAgent({ endpoint: "http://localhost", token: "data" });
    return current ? <DataRoom agent={current} /> : null;
  }
  function DataRoom({ agent: current }: { agent: Agent }) {
    agent = current;
    const [state, setState] = useState({
      version: 1,
      trackId: "chat",
      mounted: true,
      generation: 0,
    });
    update = (patch) => setState((before) => ({ ...before, ...patch }));
    const equivalent = agent;
    return (
      <>
        {state.mounted && (
          <Data
            key={state.generation}
            track={equivalent
              .room("selected-room")
              .participant("alice")
              .data(state.trackId)}
            onEvent={async (event) => {
              assert.ok(DataEvent.Message.instanceOf(event));
              calls.push(
                `${state.trackId}:${state.version}:${event.inner.sequence}`,
              );
              if (event.inner.sequence === 1n) await pending.promise;
              if (event.inner.sequence === 4n) await retired.promise;
              if (fail) throw new Error("processing failed");
            }}
            onError={(error) => {
              errors.push(`${state.version}:${(error as Error).message}`);
              throw new Error("reporting failed");
            }}
          />
        )}
        <Data
          track={agent.room("selected-room").participant("alice").data("chat")}
          onEvent={(event) => {
            assert.ok(DataEvent.Message.instanceOf(event));
            other.push(event.inner.sequence);
          }}
        />
      </>
    );
  }
  const root = createRoot(document.createElement("div"));
  const originalError = console.error;
  const diagnostics: unknown[][] = [];
  console.error = (...args) => {
    diagnostics.push(args);
  };
  try {
    await flush(() =>
      root.render(
        <StrictMode>
          <Room />
        </StrictMode>,
      ),
    );
    const room = agent.data("chat");
    assert.equal(room, agent.data("chat"));
    assert.equal("delivery" in room, false);
    assert.notEqual(room, agent.video("chat"));
    assert.notEqual(room, agent.data("other"));
    const participant = agent
      .room("selected-room")
      .participant("alice")
      .data("chat");
    assert.notEqual(room, participant);
    assert.notEqual(
      participant,
      agent.room("selected-room").participant("bob").data("chat"),
    );
    const owners = inbox.owners().map(([owner]) => owner);
    assert.equal(owners.length, 2);
    await flush(() => {
      inbox.deliver("chat", message(1n));
      inbox.deliver("chat", message(2n));
    });
    assert.deepEqual(calls, ["chat:1:1"], "one handler at a time per mount");
    assert.deepEqual(
      other,
      [1n, 2n],
      "a slow handler does not block another mount of the same track",
    );
    await flush(() => update({ version: 2 }));
    assert.deepEqual(
      inbox.owners().map(([owner]) => owner),
      owners,
      "callback and equivalent wrapper updates preserve receive owners",
    );
    await flush(() => pending.resolve());
    assert.deepEqual(
      calls,
      ["chat:1:1", "chat:2:2"],
      "the next event uses the latest committed callback",
    );

    fail = true;
    await flush(() => inbox.deliver("chat", message(3n)));
    assert.deepEqual(errors, ["2:processing failed"]);
    assert.equal(
      inbox.owners().length,
      1,
      "handler rejection releases only the failed mount even if onError throws",
    );
    assert.equal(diagnostics.length, 1);
    await flush(() => update({ version: 3 }));
    await flush(() => inbox.deliver("chat", message(5n)));
    assert.equal(
      calls.length,
      3,
      "callback changes do not restart a failed subscription",
    );
    assert.deepEqual(other, [1n, 2n, 3n, 5n]);

    fail = false;
    await flush(() => update({ generation: 1 }));
    assert.equal(inbox.owners().length, 2, "remount restarts reception");
    await flush(() => inbox.deliver("chat", message(4n)));
    await flush(() => update({ trackId: "replacement" }));
    assert.equal(
      inbox.owners().length,
      2,
      "target replacement releases a handler blocked on application work promptly",
    );
    await flush(() => inbox.deliver("replacement", message(6n)));
    assert.equal(calls.at(-1), "replacement:3:6");
    await flush(() => retired.reject(new Error("late failure")));
    assert.deepEqual(
      errors,
      ["2:processing failed"],
      "retired promise rejection cannot call the replacement error handler",
    );
    assert.equal(inbox.owners().length, 2);

    const [, reads] = inbox
      .owners()
      .find(([owner]) => owner.state().selector.trackId === "replacement")!;
    await flush(() => reads.fail(new Error("receive failed")));
    assert.equal(errors.length, 2, "receive failure reports once");
    assert.equal(inbox.owners().length, 1);
    await flush(() => update({ mounted: false }));
    assert.equal(inbox.owners().length, 1);
  } finally {
    await flush(() => root.unmount());
    assert.equal(inbox.owners().length, 0);
    inbox.restore();
    console.error = originalError;
  }
}
