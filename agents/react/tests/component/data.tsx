import assert from "node:assert/strict";
import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { Data, useAgent, type Agent, type DataEvent } from "../../src/index.js";
import {
  createReactAgent,
  managedAgent,
  trackIdentity,
} from "../../src/agent.js";
import { deferred, flush } from "./platform.js";
import { runtimes } from "./runtime.js";

const message = (sequence: bigint): DataEvent<"ordered"> => ({
  type: "message",
  participantId: "alice",
  trackId: "chat-id",
  sequence,
  data: Uint8Array.of(Number(sequence)),
});

export async function dataLifetimes() {
  let agent!: Agent;
  let update!: (
    patch: Partial<{
      version: number;
      label: string;
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
    agent = useAgent({ endpoint: "http://localhost", token: "data" });
    const [state, setState] = useState({
      version: 1,
      label: "chat",
      mounted: true,
      generation: 0,
    });
    update = (patch) => setState((before) => ({ ...before, ...patch }));
    const equivalent = createReactAgent(managedAgent(agent));
    return (
      <>
        {state.mounted && (
          <Data
            key={state.generation}
            track={equivalent.data(state.label, "ordered")}
            onEvent={async (event) => {
              assert.equal(event.type, "message");
              if (event.type !== "message") return;
              calls.push(`${state.label}:${state.version}:${event.sequence}`);
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
          track={agent.data("chat", "ordered")}
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
    const room = trackIdentity(agent.data("chat", "ordered"));
    assert.equal(
      room,
      trackIdentity(
        createReactAgent(managedAgent(agent)).data("chat", "ordered"),
      ),
    );
    assert.notEqual(room, trackIdentity(agent.data("chat", "lossy")));
    assert.notEqual(room, trackIdentity(agent.video("chat")));
    assert.notEqual(room, trackIdentity(agent.data("other", "ordered")));
    const participant = trackIdentity(
      agent.remote.participant("alice").data("chat", "ordered"),
    );
    assert.notEqual(room, participant);
    assert.notEqual(
      participant,
      trackIdentity(agent.remote.participant("bob").data("chat", "ordered")),
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
    await flush(() => update({ label: "replacement" }));
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
        runtime.dataOwners.get(command.owner)?.spec.label === "replacement"
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
    agent = useAgent({ endpoint: "http://localhost", token: "stale-data" });
    const [mounted, setMounted] = useState(false);
    mount = setMounted;
    return mounted ? (
      <Data
        track={agent.data("late", "ordered")}
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
