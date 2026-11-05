import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

export async function mediaResourceContract(pulsebeam) {
  const agent = await pulsebeam.createAgent({
    endpoint: "http://localhost",
    token: "media-resource-contract",
    logging: { level: "off" },
  });
  const borrowed = new Track("video");
  const source = pulsebeam.VideoSource.fromTrack(borrowed);
  try {
    const beforeLookup = agent.snapshot().desiredRevision;
    const local = agent.video("camera");
    assert.equal(agent.video("camera"), local);
    const other = await pulsebeam.createAgent({
      endpoint: "http://localhost",
      token: "other-media-resource-contract",
      logging: { level: "off" },
    });
    assert.notEqual(other.video("camera"), local);
    await other.close();
    assert.equal(
      agent.snapshot().desiredRevision,
      beforeLookup,
      "inert lookup cannot claim a sender",
    );

    const first = await local.publish(source, { enabled: false });
    await assert.rejects(
      local.publish(source, { enabled: true }),
      pulsebeam.ResourceOwnershipError,
    );
    assert.equal(borrowed.readyState, "live");
    await first.configure({ enabled: true });
    await first.setSource(null);
    assert.equal(
      borrowed.readyState,
      "live",
      "source withdrawal cannot stop capture",
    );
    await first.close();
    const replacement = await local.publish(source, { enabled: true });
    await replacement.close();
    assert.equal(borrowed.readyState, "live");

    const participant = agent.participant("absent");
    const video = await participant.video("camera").subscribe({
      visible: true,
      height: 360,
      minHeight: 0,
      minFps: 0,
      priority: 1,
      playoutDelay: { mode: "adaptive" },
    });
    const audio = await participant.audio("microphone").subscribe({
      playoutDelay: { mode: "adaptive" },
    });
    const selection = await agent.selectAudio({ automatic: false });
    await selection.close();
    assert.equal(
      audio.isClosed(),
      false,
      "selection close cannot release named audio ownership",
    );
    const pendingVideo = assert.rejects(video.observe(), { code: "closed" });
    const pendingAudio = assert.rejects(audio.observe(), { code: "closed" });
    const pendingPublication = agent
      .video("late")
      .publish(source, { enabled: true });
    const retiredPublication = pendingPublication.then(
      (publication) => assert.equal(publication.isClosed(), true),
      (error) => assert.equal(error.code, "closed"),
    );
    await agent.close();
    await Promise.all([pendingVideo, pendingAudio, retiredPublication]);
    assert.equal(video.isClosed(), true);
    assert.equal(audio.isClosed(), true);
    assert.equal(
      source.isClosed(),
      false,
      "Agent retirement cannot transfer caller-owned capture",
    );
  } finally {
    await agent.close();
    await source.close();
  }
}
