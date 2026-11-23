import assert from "node:assert/strict";
import { test } from "node:test";
import { DataRegistry } from "../../unit-javascript/data.js";
import { OperationError } from "../../unit-javascript/agent-observation.js";
import { ErrorCode } from "../../unit-javascript/schema.js";
import { ResourceOwners } from "../../unit-javascript/resources.js";

test("data sends preserve shared codes for local admission and backend failure", async () => {
  const owners = new ResourceOwners((error) => assert.fail(error));
  const registry = new DataRegistry(
    {
      getSnapshot: () => ({ roomId: "room", catalog: { tracks: [] } }),
      subscribe: () => () => {},
    },
    owners,
  );
  const local = registry.local("events");
  const publication = local.publish();
  const rejectsWith = (code) => (error) => {
    assert.ok(error instanceof OperationError);
    assert.equal(error.info.code, code);
    assert.equal(Object.isFrozen(error.info), true);
    return true;
  };
  await assert.rejects(
    publication.send(Uint8Array.of(1)),
    rejectsWith(ErrorCode.NotReady),
  );
  await assert.rejects(
    publication.send([1]),
    rejectsWith(ErrorCode.InvalidArgument),
  );
  const runtime = {
    resource_now(command) {
      assert.equal(command.op, "declareDataPublication");
      return { owner: 1n };
    },
    data_publication() {
      return {
        state: () => ({ observationRevision: 1n }),
        updates() {
          let resolve;
          return {
            next: () => new Promise((done) => (resolve = done)),
            close: () => resolve?.(undefined),
          };
        },
      };
    },
    async resource(command) {
      assert.equal(command.op, "release");
    },
    async send_owned_data(
      owner,
      payload,
      recipients,
      nanoseconds,
      uncertainty,
    ) {
      assert.equal(owner, 1n);
      assert.deepEqual(payload, Uint8Array.of(1));
      assert.deepEqual(recipients, ["recipient"]);
      assert.equal(nanoseconds, -(2n ** 63n));
      assert.equal(uncertainty, 0);
      throw Object.assign(new Error("private transport details"), {
        pulsebeamInfo: {
          code: ErrorCode.CapacityExceeded,
          message: "Data send queue is full",
        },
      });
    },
  };
  owners.activate(runtime);
  await assert.rejects(
    publication.send(Uint8Array.of(1), {
      recipients: ["recipient"],
      capturedAt: {
        clock: "publication",
        nanoseconds: -(2n ** 63n),
        uncertaintyNanoseconds: 0,
      },
    }),
    (error) => {
      rejectsWith(ErrorCode.CapacityExceeded)(error);
      assert.equal(error.message, "Data send queue is full");
      assert.equal(error.message.includes("private"), false);
      return true;
    },
  );
  await assert.rejects(
    publication.send(Uint8Array.of(1), {
      capturedAt: { clock: "publication", nanoseconds: 2n ** 63n },
    }),
    rejectsWith(ErrorCode.InvalidArgument),
  );
  await registry.close();
  await assert.rejects(
    publication.send(Uint8Array.of(1)),
    rejectsWith(ErrorCode.Closed),
  );
});

test("exact-source failures wake pending reads and preserve their shared cause after close", async () => {
  let failed = false;
  let released = 0;
  const info = {
    code: ErrorCode.PermissionDenied,
    message: "Source reception is not authorized",
  };
  const owners = new ResourceOwners((error) => assert.fail(error));
  const registry = new DataRegistry(
    {
      getSnapshot: () => ({ roomId: "room", catalog: { tracks: [] } }),
      subscribe: () => () => {},
    },
    owners,
  );
  const runtime = {
    resource_now(command) {
      if (command.op === "release") {
        assert.equal(command.owner, 1n);
        return;
      }
      assert.equal(command.op, "receiveData");
      return { owner: 1n };
    },
    data_subscription(owner) {
      assert.equal(owner, 1n);
      return {
        state: () => ({ observationRevision: 1n }),
        updates() {
          let resolve;
          return {
            next: () => new Promise((done) => (resolve = done)),
            close: () => resolve?.(undefined),
          };
        },
      };
    },
    async resource(command) {
      if (command.op === "release") {
        released++;
        return;
      }
      assert.equal(command.op, "readData");
      if (failed)
        throw Object.assign(new Error(info.message), { pulsebeamInfo: info });
      return null;
    },
  };
  owners.activate(runtime);
  const receiver = registry.subscribe(
    { kind: "data", trackId: "events", delivery: "ordered" },
    { room: "room", participantId: "alice", trackId: "events" },
  );
  const pending = receiver.next();
  failed = true;
  assert.equal(registry.notify({ type: "data-ready", owners: [1n] }), true);
  let terminal;
  await assert.rejects(pending, (error) => {
    terminal = error;
    assert.ok(error instanceof OperationError);
    assert.deepEqual(error.info, info);
    assert.equal(Object.isFrozen(error.info), true);
    return true;
  });
  await receiver.close();
  await assert.rejects(receiver.next(), (error) => error === terminal);
  assert.equal(released, 1);
});
