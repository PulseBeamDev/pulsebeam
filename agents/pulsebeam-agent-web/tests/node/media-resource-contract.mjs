import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

export async function mediaResourceContract(pulsebeam) {
  const agent = pulsebeam.createAgent({
    endpoint: "http://localhost",
    token: "media-resource-contract",
    topology: { localVideos: 1, localAudios: 0 },
    logging: { level: "off" },
  });
  const beforeLookup = agent.getSnapshot().desiredRevision;
  const descriptor = pulsebeam.track.video("camera");
  const local = agent.track(descriptor);
  assert.equal(agent.track(pulsebeam.track.video("camera")), local);
  const other = pulsebeam.createAgent({
    endpoint: "http://localhost",
    token: "other-media-resource-contract",
    topology: { localVideos: 1, localAudios: 0 },
    logging: { level: "off" },
  });
  assert.notEqual(other.track(descriptor), local);
  other.close();
  assert.equal(
    agent.getSnapshot().desiredRevision,
    beforeLookup,
    "inert descriptor lookup cannot claim a sender",
  );

  const borrowed = new Track("video");
  const source = pulsebeam.createCaptureSource(borrowed, "video");
  const first = pulsebeam.attachLocalTrack(local, { source, enabled: false });
  await first.ready;
  const duplicate = pulsebeam.attachLocalTrack(local, { source });
  await assert.rejects(duplicate.ready, pulsebeam.ResourceOwnershipError);
  assert.equal(borrowed.readyState, "live");

  first.setEnabled(true);
  first.setSource(null);
  assert.equal(
    borrowed.readyState,
    "live",
    "publication teardown cannot stop capture",
  );
  await first.close();

  const replacement = pulsebeam.attachLocalTrack(local, { source });
  await replacement.ready;
  await replacement.close();
  assert.equal(borrowed.readyState, "live");
  agent.close();
}
