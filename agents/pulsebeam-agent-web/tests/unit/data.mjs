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
  const local = registry.local({
    kind: "data",
    trackId: "events",
    delivery: "ordered",
  });
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
    async send_owned_data() {
      throw Object.assign(new Error("private transport details"), {
        pulsebeamInfo: {
          code: ErrorCode.CapacityExceeded,
          message: "Data send queue is full",
        },
      });
    },
  };
  owners.activate(runtime);
  await assert.rejects(publication.send(Uint8Array.of(1)), (error) => {
    rejectsWith(ErrorCode.CapacityExceeded)(error);
    assert.equal(error.message, "Data send queue is full");
    assert.equal(error.message.includes("private"), false);
    return true;
  });
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
  const pending = [receiver.next(), receiver.next()];
  const outcomes = Promise.allSettled(pending);
  failed = true;
  assert.equal(registry.notify({ type: "data-ready", owners: [1n] }), true);
  const results = await outcomes;
  for (const result of results) {
    assert.equal(result.status, "rejected");
    assert.ok(result.reason instanceof OperationError);
    assert.deepEqual(result.reason.info, info);
    assert.equal(Object.isFrozen(result.reason.info), true);
  }
  assert.equal(results[0].reason, results[1].reason);
  await receiver.close();
  await assert.rejects(receiver.next(), (error) => error === results[0].reason);
  assert.equal(released, 1);
});
