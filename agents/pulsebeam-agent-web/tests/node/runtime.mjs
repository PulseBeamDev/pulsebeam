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
  await agent.connect();
  await recovered;
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
  const runtime = new BrowserRuntime(config);
  assert.deepEqual(runtime.diagnostics(), {
    peers: 0,
    requests: 0,
    timers: 0,
    closed: false,
    lastError: undefined,
  });
  runtime.renew_authorization("renewed");
  assert.equal(runtime.snapshot().connection, "disconnected");
  runtime.close();
  assert.equal(runtime.diagnostics().closed, true);
  assert.throws(() => runtime.connect(), /closed/);
  assert.throws(() => runtime.set_owned_muted(1n, true), /closed/);
  assert.throws(
    () => runtime.replace_owned_track(1n, null, {}, false, true),
    /closed/,
  );
  runtime.abort();
  runtime.free();
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
    "emitted WASM initialization, diagnostics, authorization, close fencing and free passed",
  );
}
