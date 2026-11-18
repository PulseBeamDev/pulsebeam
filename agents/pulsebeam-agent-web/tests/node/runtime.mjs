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
  const NativeModule = WebAssembly.Module;
  WebAssembly.Module = class extends NativeModule {
    constructor(source) {
      attempts++;
      throw new WebAssembly.CompileError(
        "controlled artifact compilation failure",
      );
    }
  };
  const { Agent, ErrorCode, OperationError } = await import(
    "../../node_assets/dist/index.js"
  );
  const initializationFailure = (error) => {
    assert.equal(error instanceof OperationError, true);
    assert.equal(error.info.code, ErrorCode.Internal);
    assert.equal(error.message, "Browser runtime initialization failed");
    return true;
  };
  assert.throws(() => new Agent(config), initializationFailure);
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
  await assert.rejects(agent.connect(), initializationFailure);
  assert.equal((await failure).class, "initialization");
  const observed = agent.getSnapshot().failure;
  await assert.rejects(agent.disconnect(), initializationFailure);
  assert.equal(agent.getSnapshot().failure, observed);
  assert.equal(agent.connection, "terminal-failure");
  await import("./platform.mjs");
  WebAssembly.Module = class extends NativeModule {
    constructor(source) {
      attempts++;
      super(source);
    }
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
    4,
    "constructor and managed activation fail synchronously; explicit connects retry initialization",
  );
  assert.equal(agent.error, null);
  await agent.close();
  console.log(
    "artifact compilation failure persists through state updates and explicit connect retries initialization",
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
  let requests = 0;
  globalThis.fetch = async () => {
    requests++;
    return new Response(bytes, {
      headers: { "Content-Type": "application/wasm" },
    });
  };
  const NativeModule = WebAssembly.Module;
  const NativeInstance = WebAssembly.Instance;
  const nativeInstantiate = WebAssembly.instantiate;
  let compilations = 0;
  let instances = 0;
  let asynchronousInstantiations = 0;
  WebAssembly.Instance = class extends NativeInstance {
    constructor(module, imports) {
      instances++;
      super(module, imports);
    }
  };
  WebAssembly.instantiate = (...args) => {
    asynchronousInstantiations++;
    return Reflect.apply(nativeInstantiate, WebAssembly, args);
  };
  WebAssembly.Module = class extends NativeModule {
    constructor(source) {
      compilations++;
      super(source);
    }
  };
  const { Agent, AgentStatus, ErrorCode } = await import(
    "../../node_assets/dist/index.js"
  );
  const { BrowserRuntime, whenInitialized } = await import(
    "../../node_assets/dist/wasm.js"
  );
  assert.deepEqual(
    [compilations, instances, asynchronousInstantiations],
    [0, 0, 0],
    "root import does not compile or instantiate WASM",
  );
  const agent = new Agent(config);
  assert.equal(agent instanceof Agent, true);
  assert.equal(agent instanceof Promise, false);
  assert.equal(agent.state().status, AgentStatus.Idle);
  assert.equal(agent.snapshot().desiredRoom, null);
  assert.equal(agent.snapshot().connection, "disconnected");
  assert.equal(requests, 0, "constructor does not fetch or connect");
  const initialization = whenInitialized();
  const inert = new BrowserRuntime(config);
  assert.equal(
    inert.diagnostics().peers,
    0,
    "Core exists before awaiting initialization",
  );
  inert.close();
  await agent.close();
  await Promise.all([initialization, whenInitialized(), whenInitialized()]);
  assert.deepEqual(
    [compilations, instances, asynchronousInstantiations],
    [1, 1, 0],
    "private bootstrap creates one synchronous Core instance",
  );
  assert.equal(requests, 0, "local bootstrap does not fetch");
  WebAssembly.Module = NativeModule;
  WebAssembly.Instance = NativeInstance;
  WebAssembly.instantiate = nativeInstantiate;
  const invalidConfiguration = (error) => {
    assert.equal(error instanceof Error, true);
    assert.equal(error.pulsebeamInfo.code, ErrorCode.InvalidArgument);
    assert.equal(error.message.includes("private-configuration-value"), false);
    return true;
  };
  assert.throws(
    () => new BrowserRuntime({ ...config, token: "t".repeat(8193) }),
    invalidConfiguration,
  );
  assert.throws(
    () =>
      new BrowserRuntime({
        ...config,
        endpoint: "private-configuration-value",
      }),
    invalidConfiguration,
  );
  assert.throws(
    () =>
      new BrowserRuntime({
        ...config,
        token: { private: "private-configuration-value" },
      }),
    invalidConfiguration,
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
