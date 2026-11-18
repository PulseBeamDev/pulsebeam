import assert from "node:assert/strict";

export async function catalogContract(BrowserRuntime, ErrorCode, pulsebeam) {
  const config = { endpoint: "http://localhost", token: "catalog-owner" };
  const classified = (code) => (error) => error.pulsebeamInfo?.code === code;
  for (const abort of [false, true]) {
    const runtime = new BrowserRuntime(config);
    const cursor = Uint8Array.from({ length: 32 }, (_, index) => index);
    const watch = runtime.catalog_watch("other-room", 7, cursor);
    cursor.fill(255);
    const initial = watch.state();
    assert.equal(initial.status, 0);
    assert.equal(initial.room, "other-room");
    assert.equal(initial.limit, 7);
    assert.equal(initial.page, undefined, "loading is not an empty live page");
    assert.deepEqual(
      initial.cursor,
      Array.from({ length: 32 }, (_, i) => i),
    );
    assert.equal(Object.isFrozen(initial), true);
    assert.equal(Object.isFrozen(initial.cursor), true);
    assert.equal(watch.state(), initial, "same-revision projection is shared");
    assert.equal(runtime.diagnostics().peers, 0, "watch does not join");
    const first = watch.updates();
    const second = watch.updates();
    assert.equal(await first.next(), initial);
    assert.equal(await second.next(), initial);
    const remaining = Array.from({ length: 30 }, () => watch.updates());
    assert.throws(
      () => watch.updates(),
      classified(ErrorCode.CapacityExceeded),
    );
    remaining.pop().free();
    const reused = watch.updates();
    const pending = first.next();
    await assert.rejects(first.next(), classified(ErrorCode.Conflict));
    const independent = second.next();
    first.close();
    assert.equal(await pending, undefined);
    assert.equal(watch.state(), initial, "cursor close retains watch demand");
    if (abort) runtime.abort();
    else runtime.close();
    const final = await independent;
    assert.equal(final.status, 3);
    assert.equal(watch.state(), final);
    assert.equal(initial.status, 0, "retained snapshots remain stable");
    assert.equal(await second.next(), undefined);
    watch.close();
    watch.close();
    assert.equal(watch.state(), final);
    for (const updates of [first, second, reused, ...remaining]) updates.free();
    watch.free();
    runtime.free();
  }
  const { Agent, OperationError, WatchStatus } = pulsebeam;
  const agent = new Agent(config);
  const room = agent.room("pinned-room");
  const defaultWatch = room.watch();
  assert.equal(defaultWatch instanceof Promise, false);
  assert.equal(defaultWatch.state().limit, 100);
  assert.equal(defaultWatch.state().status, WatchStatus.Loading);
  assert.equal(agent.snapshot().desiredRoom, null);
  const bytes = Uint8Array.from({ length: 32 }, (_, i) => i);
  const watch = room.watch({ limit: 7, cursor: bytes });
  bytes.fill(255);
  assert.equal(watch.state().room, "pinned-room");
  assert.equal(watch.state().limit, 7);
  assert.deepEqual(
    watch.state().cursor,
    Array.from({ length: 32 }, (_, i) => i),
  );
  for (const config of [
    { limit: 501 },
    { limit: -1 },
    { limit: 1.5 },
    { limit: NaN },
    { cursor: [256] },
    { cursor: [-1] },
  ]) {
    assert.throws(
      () => room.watch(config),
      (error) =>
        error instanceof OperationError &&
        error.info.code === ErrorCode.InvalidArgument,
    );
  }
  const publicWatches = Array.from({ length: 6 }, () => room.watch());
  assert.throws(
    () => room.watch(),
    (error) =>
      error instanceof OperationError &&
      error.info.code === ErrorCode.CapacityExceeded,
  );
  const closing = defaultWatch.close();
  const replacementWatch = room.watch();
  assert.equal(defaultWatch.state().status, WatchStatus.Closed);
  await closing;
  const independent = await room
    .participant("publisher")
    .data("events", "ordered")
    .subscribe();
  const first = watch.updates();
  const second = watch.updates();
  assert.equal(await first.next(), watch.state());
  assert.equal(await second.next(), watch.state());
  const final = first.next();
  void watch.close();
  assert.equal((await final).status, WatchStatus.Closed);
  assert.equal(await first.next(), undefined);
  assert.equal(
    independent.isClosed(),
    false,
    "watch close leaves independent subscription owned",
  );
  await independent.close();
  await agent.close();
  assert.throws(
    () => room.watch(),
    (error) =>
      error instanceof OperationError && error.info.code === ErrorCode.Closed,
  );
  for (const owner of [...publicWatches, replacementWatch, watch])
    await owner.close();
  first.close();
  second.close();
  const { createManagedAgent } = await import(
    "../../node_assets/dist/agent.js"
  );
  const managed = createManagedAgent(config);
  const managedRoom = managed.room("managed-room");
  assert.throws(
    () => managedRoom.watch(),
    (error) =>
      error instanceof OperationError && error.info.code === ErrorCode.NotReady,
  );
  managed.activate();
  const oldWatch = managedRoom.watch();
  managed.deactivate();
  assert.equal(oldWatch.state().status, WatchStatus.Closed);
  assert.throws(
    () => managedRoom.watch(),
    (error) =>
      error instanceof OperationError && error.info.code === ErrorCode.NotReady,
  );
  managed.activate();
  const newWatch = managedRoom.watch();
  assert.equal(newWatch.state().status, WatchStatus.Loading);
  await oldWatch.close();
  assert.equal(newWatch.state().status, WatchStatus.Loading);
  await newWatch.close();
  await managed.close();

  const runtime = new BrowserRuntime(config);
  const watches = Array.from({ length: 8 }, () =>
    runtime.catalog_watch("room", 100, new Uint8Array()),
  );
  assert.throws(
    () => runtime.catalog_watch("room", 100, new Uint8Array()),
    classified(ErrorCode.CapacityExceeded),
  );
  watches.pop().free();
  const replacement = runtime.catalog_watch("room", 100, new Uint8Array());
  replacement.close();
  replacement.free();
  for (const watch of watches) watch.free();
  runtime.close();
  assert.throws(
    () => runtime.catalog_watch("room", 100, new Uint8Array()),
    classified(ErrorCode.Closed),
  );
  runtime.free();
}
