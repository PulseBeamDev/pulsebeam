import assert from "node:assert/strict";
import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { Data, useAgent, type Agent, type DataEvent } from "../../src/index.js";
import { deferred, flush } from "./platform.js";
import { runtimes } from "./runtime.js";

const message = (sequence: bigint): DataEvent => ({
  type: "message",
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
              assert.equal(event.type, "message");
              if (event.type !== "message") return;
              calls.push(`${state.trackId}:${state.version}:${event.sequence}`);
              if (event.sequence === 1n) await pending.promise;
              if (event.sequence === 4n) await retired.promise;
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
            assert.equal(event.type, "message");
            if (event.type !== "message") return;
            other.push(event.sequence);
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
    const runtime = runtimes.at(-1)!;
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
    const owners = [...runtime.dataOwners.keys()];
    assert.equal(owners.length, 2);
    await flush(() => {
      runtime.deliverData("chat", message(1n));
      runtime.deliverData("chat", message(2n));
    });
    assert.deepEqual(calls, ["chat:1:1"], "one handler at a time per mount");
    assert.deepEqual(
      other,
      [1n, 2n],
      "a slow handler does not block another mount of the same track",
    );
    await flush(() => update({ version: 2 }));
    assert.deepEqual(
      [...runtime.dataOwners.keys()],
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
    await flush(() => runtime.deliverData("chat", message(3n)));
    assert.deepEqual(errors, ["2:processing failed"]);
    assert.equal(
      runtime.dataOwners.size,
      1,
      "handler rejection releases only the failed mount even if onError throws",
    );
    assert.equal(diagnostics.length, 1);
    await flush(() => update({ version: 3 }));
    await flush(() => runtime.deliverData("chat", message(5n)));
    assert.equal(
      calls.length,
      3,
      "callback changes do not restart a failed subscription",
    );
    assert.deepEqual(other, [1n, 2n, 3n, 5n]);

    fail = false;
    await flush(() => update({ generation: 1 }));
    assert.equal(runtime.dataOwners.size, 2, "remount restarts reception");
    await flush(() => runtime.deliverData("chat", message(4n)));
    await flush(() => update({ trackId: "replacement" }));
    assert.equal(
      runtime.dataOwners.size,
      2,
      "target replacement releases a handler blocked on application work promptly",
    );
    await flush(() => runtime.deliverData("replacement", message(6n)));
    assert.equal(calls.at(-1), "replacement:3:6");
    await flush(() => retired.reject(new Error("late failure")));
    assert.deepEqual(
      errors,
      ["2:processing failed"],
      "retired promise rejection cannot call the replacement error handler",
    );
    assert.equal(runtime.dataOwners.size, 2);

    const resource = runtime.resource.bind(runtime);
    let rejectRead = true;
    runtime.resource = async (command) => {
      if (
        command.op === "readData" &&
        rejectRead &&
        runtime.dataOwners.get(command.owner)?.spec.trackId === "replacement"
      ) {
        rejectRead = false;
        throw new Error("receive failed");
      }
      return resource(command);
    };
    await flush(() => runtime.deliverData("replacement", message(7n)));
    assert.equal(errors.length, 2, "receive failure reports once");
    assert.equal(runtime.dataOwners.size, 1);
    await flush(() => update({ mounted: false }));
    assert.equal(runtime.dataOwners.size, 1);
  } finally {
    await flush(() => root.unmount());
    assert.equal(runtimes.at(-1)!.dataOwners.size, 0);
    console.error = originalError;
  }
}

export async function staleDataClaims() {
  let agent!: Agent;
  let mount!: (value: boolean) => void;
  const completion = deferred<void>();
  const calls: unknown[] = [];
  function Room() {
    const current = useAgent({
      endpoint: "http://localhost",
      token: "stale-data",
    });
    return current ? <StaleDataRoom agent={current} /> : null;
  }
  function StaleDataRoom({ agent }: { agent: Agent }) {
    const [mounted, setMounted] = useState(false);
    mount = setMounted;
    return mounted ? (
      <Data
        track={agent.room("selected-room").participant("alice").data("late")}
        onEvent={(event) => {
          calls.push(event);
        }}
        onError={(error) => {
          calls.push(error);
        }}
      />
    ) : null;
  }
  const root = createRoot(document.createElement("div"));
  await flush(() => root.render(<Room />));
  const runtime = runtimes.at(-1)!;
  const resource = runtime.resource.bind(runtime);
  runtime.resource = async (command) => {
    if (command.op === "receiveData") await completion.promise;
    return resource(command);
  };
  await flush(() => mount(true));
  await flush(() => mount(false));
  await flush(() => completion.resolve());
  assert.equal(
    runtime.dataOwners.size,
    0,
    "late receive claim is released after retirement",
  );
  assert.deepEqual(calls, []);
  await flush(() => root.unmount());
}
