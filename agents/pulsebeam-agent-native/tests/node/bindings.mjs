import assert from "node:assert/strict";
import * as core from "../../node_assets/pulsebeam_agent_core.js";
import * as native from "../../node_assets/pulsebeam_agent_native.js";

const agent = await native.Agent.create({
  session: {
    endpoint: "http://127.0.0.1:1",
    token: "binding-test",
    retry: { initialDelayMs: 100n, maximumDelayMs: 1000n, maximumAttempts: 2 },
    logLevel: core.LogLevel.Warn,
  },
  media: {
    audioInput: new native.AudioInput.Pcm(),
    videoInput: native.VideoInput.I420,
    videoPublications: [],
  },
});
try {
  const publication = await agent.publication(new core.TrackSpec.Video({ label: "camera" }));
  assert.equal(publication.isClosed(), false);
  await publication.close();
  assert.equal(publication.isClosed(), true);
  await assert.rejects(publication.input(), native.NativeError.Closed);
} finally {
  await agent.abort();
}
assert.equal(agent.snapshot().connection.tag, "Disconnected");
