import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";

const bytes = await readFile(
  new URL(
    "../../node_assets/dist/wasm/pulsebeam_agent_web_bg.wasm",
    import.meta.url,
  ),
);
const config = {
  endpoint: "http://localhost",
  token: "node-lifecycle",
};
if (process.argv[2] === "initialization-failure") {
  let attempts = 0;
  globalThis.fetch = async () => {
    attempts++;
    throw new Error("controlled artifact fetch failure");
  };
  const { createAgent } = await import("../../node_assets/dist/index.js");
  await assert.rejects(
    createAgent(config),
    /controlled artifact fetch failure/,
  );
  const { createManagedAgent } = await import(
    "../../node_assets/dist/internal/react.js"
  );
  const agent = createManagedAgent(config);
  const failure = new Promise((resolve) =>
    agent.subscribe(() => {
      if (agent.connection === "terminal-failure")
        resolve(agent.getSnapshot().failure);
    }),
  );
  agent.activate();
  agent.join("initialization-retry-room");
  await assert.rejects(agent.connect(), /controlled artifact fetch failure/);
  const observed = await failure;
  assert.equal(observed.class, "initialization");
  await assert.rejects(agent.disconnect(), /controlled artifact fetch failure/);
  assert.equal(agent.getSnapshot().failure, observed);
  assert.equal(agent.connection, "terminal-failure");
  await import("./platform.mjs");
  globalThis.fetch = async () => {
    attempts++;
    return new Response(bytes, {
      headers: { "Content-Type": "application/wasm" },
    });
  };
  const recovered = new Promise((resolve) => {
    const unsubscribe = agent.subscribe(() => {
      if (agent.getSnapshot().desiredRevision > 0) {
        unsubscribe();
        resolve();
      }
    });
  });
  agent.join("initialization-retry-room");
  await agent.connect();
  await recovered;
  assert.equal(agent.snapshot().desiredRoom, "initialization-retry-room");
  assert.equal(
    attempts,
    3,
    "public creation rejects and managed explicit connect retries initialization",
  );
  assert.equal(agent.error, null);
  await agent.close();
  console.log(
    "artifact fetch failure persists through state updates and explicit connect retries initialization",
  );
} else if (process.argv[2] === "raw-loader-race") {
  const { default: initialize, BrowserRuntime } = await import(
    "../../node_assets/dist/wasm/pulsebeam_agent_web.js"
  );
  let release;
  const slow = initialize({
    module_or_path: new Promise((resolve) => {
      release = resolve;
    }),
  });
  await initialize({ module_or_path: bytes });
  const runtime = new BrowserRuntime(config);
  assert.equal(runtime.diagnostics().peers, 0);
  release(bytes);
  await slow;
  // This is the original fixture fault: an overlapping generated initialization
  // replaces the instance behind a receiver allocated by the other instance.
  assert.throws(() => runtime.diagnostics(), WebAssembly.RuntimeError);
  console.log(
    "overlapping raw initialization reproduces the invalid receiver trap",
  );
} else {
  const failedInitialization = spawnSync(
    process.execPath,
    [import.meta.filename, "initialization-failure"],
    { encoding: "utf8" },
  );
  assert.equal(
    failedInitialization.status,
    0,
    failedInitialization.stderr + failedInitialization.stdout,
  );
  console.log(failedInitialization.stdout.trim());
  const counterexample = spawnSync(
    process.execPath,
    [import.meta.filename, "raw-loader-race"],
    { encoding: "utf8" },
  );
  assert.equal(
    counterexample.status,
    0,
    counterexample.stderr + counterexample.stdout,
  );
  console.log(counterexample.stdout.trim());
  let requests = 0;
  globalThis.fetch = async () => {
    requests++;
    return new Response(bytes, {
      headers: { "Content-Type": "application/wasm" },
    });
  };
  const { BrowserRuntime, whenInitialized } = await import(
    "../../node_assets/dist/wasm.js"
  );
  await Promise.all([whenInitialized(), whenInitialized(), whenInitialized()]);
  assert.equal(
    requests,
    1,
    "production loader shares one in-flight initialization",
  );
  assert.throws(
    () => new BrowserRuntime({ ...config, token: "t".repeat(8193) }),
  );
  const runtime = new BrowserRuntime({ ...config, token: "t".repeat(8192) });
  assert.deepEqual(runtime.diagnostics(), {
    peers: 0,
    requests: 0,
    timers: 0,
    closed: false,
    lastError: undefined,
  });
  runtime.renew_authorization("r".repeat(8192));
  assert.throws(() => runtime.renew_authorization("r".repeat(8193)));
  assert.equal(runtime.diagnostics().lastError, undefined);
  runtime.renew_authorization("renewed");
  assert.equal(runtime.snapshot().connection, "disconnected");
  assert.equal(runtime.snapshot().joinRevision, 0n);
  assert.equal(runtime.snapshot().credentialRevision, 0n);
  assert.equal(runtime.snapshot().acceptedCredentialRevision, undefined);
  assert.equal(runtime.snapshot().credentialOutcome, undefined);
  assert.equal(runtime.snapshot().desiredRoom, undefined);
  const beforeJoin = runtime.snapshot();
  assert.throws(() => runtime.join("bad\0room"));
  assert.deepEqual(runtime.snapshot(), beforeJoin);
  const snapshots = [];
  runtime.set_snapshot_listener((snapshot) => snapshots.push(snapshot));
  assert.equal(runtime.join("room-a"), undefined);
  const roomA = runtime.snapshot();
  assert.equal(roomA.desiredRoom, "room-a");
  assert.equal(roomA.joinRevision, 1n);
  runtime.join("room-b");
  assert.equal(runtime.snapshot().desiredRoom, "room-b");
  assert.equal(runtime.snapshot().joinRevision, 2n);
  assert.equal(roomA.desiredRoom, "room-a", "snapshots retain owned values");
  assert.equal(snapshots.length, 1, "join does not invoke callbacks inline");
  let reentrantSnapshot;
  runtime.set_snapshot_listener((snapshot) => {
    reentrantSnapshot = snapshot;
    runtime.join("from-listener");
  });
  assert.equal(reentrantSnapshot.desiredRoom, "room-b");
  assert.equal(runtime.snapshot().desiredRoom, "from-listener");
  assert.equal(runtime.snapshot().joinRevision, 3n);
  assert.equal(runtime.diagnostics().peers, 0);
  assert.equal(runtime.diagnostics().requests, 0);
  assert.equal(runtime.diagnostics().timers, 0);
  runtime.set_snapshot_listener(undefined);
  runtime.close();
  assert.equal(runtime.diagnostics().closed, true);
  assert.throws(() => runtime.connect(), /closed/);
  assert.throws(() => runtime.join("after-close"), /closed/);
  assert.throws(() => runtime.set_owned_muted(1n, true), /closed/);
  assert.throws(
    () => runtime.replace_owned_track(1n, null, {}, false, true),
    /closed/,
  );
  runtime.abort();
  runtime.free();
  const observed = new BrowserRuntime(config);
  const initialState = observed.state();
  const first = observed.updates();
  const independent = observed.updates();
  assert.deepEqual(await first.next(), initialState);
  assert.deepEqual(await independent.next(), initialState);
  assert.equal(typeof initialState.observationRevision, "bigint");
  const pendingState = first.next();
  observed.renew("state-credential");
  const renewedState = await pendingState;
  assert.equal(renewedState.desiredCredentialRevision, 1n);
  assert.equal(renewedState.evaluatedCredentialRevision, 0n);
  assert.equal(initialState.desiredCredentialRevision, 0n);
  assert.deepEqual(observed.state(), renewedState);
  const retiredRead = first.next();
  first.close();
  first.close();
  assert.equal(await retiredRead, undefined);
  observed.close();
  const finalState = await independent.next();
  assert.notEqual(finalState.status, initialState.status);
  assert.equal(finalState.room, undefined);
  assert.equal(finalState.desiredCredentialRevision, 1n);
  assert.equal(await independent.next(), undefined);
  assert.deepEqual(observed.state(), finalState);
  independent.close();
  first.free();
  independent.free();
  observed.free();
  const publicationRuntime = new BrowserRuntime(config);
  const claim = await publicationRuntime.resource({
    op: "claimPublication",
    spec: { kind: "video", label: "camera" },
  });
  const publication = publicationRuntime.publication(claim.owner);
  const publicationUpdates = publication.updates();
  const independentPublicationUpdates = publication.updates();
  const initialPublication = await publicationUpdates.next();
  assert.deepEqual(
    await independentPublicationUpdates.next(),
    initialPublication,
  );
  assert.equal(initialPublication.admission.status, 0);
  assert.equal(initialPublication.admission.demand, 0);
  assert.equal(initialPublication.inputAttached, false);
  assert.equal(initialPublication.configuration.desiredRevision, 1n);
  const pendingPublication = publicationUpdates.next();
  const pendingIndependentPublication = independentPublicationUpdates.next();
  await publicationRuntime.set_owned_muted(claim.owner, true);
  const configuredPublication = await pendingPublication;
  assert.equal(configuredPublication.configuration.desiredRevision, 2n);
  assert.equal(publication.state().configuration.desiredRevision, 2n);
  assert.deepEqual(await pendingIndependentPublication, configuredPublication);
  const retainedPublicationUpdates = publication.updates();
  publication.free();
  await publicationRuntime.resource({ op: "release", owner: claim.owner });
  const finalPublication = await retainedPublicationUpdates.next();
  assert.equal(finalPublication.closed, true);
  assert.equal((await independentPublicationUpdates.next()).closed, true);
  publicationRuntime.abort();
  assert.equal(await retainedPublicationUpdates.next(), undefined);
  publicationUpdates.close();
  independentPublicationUpdates.close();
  publicationUpdates.free();
  independentPublicationUpdates.free();
  retainedPublicationUpdates.free();
  publicationRuntime.free();
  const { localOperationContract } = await import("./local-operations.mjs");
  const local = await localOperationContract(BrowserRuntime);
  const { publicContract } = await import("./public-contract.mjs");
  assert.equal(
    await publicContract(await import("../../node_assets/dist/index.js")),
    true,
  );
  const { resourceContract } = await import("./resource-contract.mjs");
  await resourceContract(BrowserRuntime);
  const pulsebeam = await import("../../node_assets/dist/index.js");
  const { dataResourceContract } = await import("./data-resource-contract.mjs");
  await dataResourceContract(pulsebeam);
  const { mediaResourceContract } = await import(
    "./media-resource-contract.mjs"
  );
  await mediaResourceContract(pulsebeam);
  console.log(
    "emitted WASM authoritative binding, lossless u64 events and admission cancellation passed",
  );
  await import("./media-handles.mjs");
  assert.equal(local, true);
  console.log(
    "emitted Rust runtime source ownership, mute persistence, slot reuse and close fencing passed",
  );
  console.log(
    "emitted WASM initialization, synchronous room intent, owned snapshots, callback reentry, authorization and close fencing passed",
  );
}
