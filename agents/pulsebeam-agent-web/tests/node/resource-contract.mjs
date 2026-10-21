import assert from "node:assert/strict";

export async function resourceContract(BrowserRuntime) {
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "resource-ownership",
    logLevel: "off",
    topology: {
      localVideos: 1,
      localAudios: 0,
      remoteVideos: 1,
      remoteAudios: 0,
    },
  });
  const resource = (command) => runtime.resource(command);
  const camera = { kind: "video", label: "camera" };
  const screen = { kind: "video", label: "screen" };
  try {
    const first = await resource({ op: "claimPublication", spec: camera });
    assert.equal(typeof first.owner, "bigint");
    assert.equal(first.slot, "v0");
    await assert.rejects(resource({ op: "claimPublication", spec: camera }), {
      pulsebeamCode: "ownership",
    });
    await assert.rejects(resource({ op: "claimPublication", spec: screen }), {
      pulsebeamCode: "capacity",
    });
    await resource({
      op: "updatePublication",
      owner: first.owner,
      present: true,
    });
    const audio = await resource({ op: "claimAudio" });
    await assert.rejects(resource({ op: "claimAudio" }), {
      pulsebeamCode: "ownership",
    });
    await resource({ op: "release", owner: first.owner });
    const second = await resource({ op: "claimPublication", spec: screen });
    assert.equal(second.slot, first.slot);
    assert.notEqual(second.owner, first.owner);
    await assert.rejects(
      resource({ op: "updatePublication", owner: first.owner, present: true }),
      { pulsebeamCode: "released" },
    );
    await resource({ op: "release", owner: first.owner });
    await resource({
      op: "updatePublication",
      owner: second.owner,
      present: true,
    });
    const spec = { kind: "data", label: "events", delivery: "ordered" };
    const receiver = await resource({ op: "receiveData", spec });
    assert.equal(
      await resource({ op: "readData", owner: receiver.owner }),
      null,
    );
    await assert.rejects(resource({ op: "retainProducer", spec }), {
      pulsebeamCode: "not-connected",
    });
    await resource({ op: "release", owner: receiver.owner });
    await assert.rejects(resource({ op: "readData", owner: receiver.owner }), {
      pulsebeamCode: "released",
    });
    await resource({ op: "release", owner: audio.owner });
    const replacementAudio = await resource({ op: "claimAudio" });
    await resource({ op: "release", owner: audio.owner });
    await assert.rejects(resource({ op: "claimAudio" }), {
      pulsebeamCode: "ownership",
    });
    await resource({ op: "release", owner: replacementAudio.owner });
    const concurrent = await Promise.all(
      Array.from({ length: 40 }, () => resource({ op: "receiveData", spec })),
    );
    assert.equal(runtime.snapshot().topics.subscribers.length, 1);
    await Promise.all(
      concurrent.map(({ owner }) => resource({ op: "release", owner })),
    );
    assert.deepEqual(
      runtime.snapshot().topics.subscribers,
      [],
      "mailbox backpressure cannot discard owner release",
    );
    runtime.replace_desired({
      connected: false,
      topics: Array.from({ length: 64 }, (_, index) => ({
        name: `legacy${index}`,
        mode: "lossy",
        publish: true,
      })),
    });
    await assert.rejects(resource({ op: "receiveData", spec }), {
      pulsebeamCode: "capacity",
    });
    assert.deepEqual(runtime.snapshot().topics.subscribers, []);
    assert.equal(
      runtime.diagnostics().peers,
      0,
      "resource ownership alone does not establish transport",
    );
  } finally {
    const unfinished = resource({ op: "claimAudio" });
    runtime.close();
    runtime.abort();
    runtime.free();
    await assert.rejects(unfinished, /closed|canceled|running/);
  }
}
