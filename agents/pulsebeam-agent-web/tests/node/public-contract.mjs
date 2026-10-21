import { Track } from "./platform.mjs";

export async function publicContract(pulsebeam) {
  const { attachLocalTrack, createAgent, createCaptureSource, track } =
    pulsebeam;
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const agent = createAgent({
    endpoint: "http://localhost",
    token: "public-smoke",
    topology: { localVideos: 1, localAudios: 1 },
    logging: { level: "off" },
  });
  const closedDuringInitialization = createAgent({
    endpoint: "http://localhost",
    token: "close-before-initialization",
  });
  closedDuringInitialization.close();
  const borrowed = new Track("video");
  try {
    agent.connect();
    agent.disconnect();
    const camera = agent.track(track.video("camera"));
    assert(
      agent.track(track.video("camera")) === camera,
      "stable local handle",
    );
    assert(!("local" in agent), "callable local root was removed");
    assert(!("topic" in agent), "JSON topic facade was removed");
    assert(!("setState" in agent), "desired-state mutation was removed");
    assert(!("reconnect" in agent), "second retry operation was removed");
    assert(!("videoTracks" in agent.remote), "parallel video root was removed");
    const owner = attachLocalTrack(camera, {
      source: createCaptureSource(borrowed, "video"),
    });
    await owner.ready;
    await owner.close();
    assert(borrowed.readyState === "live", "publication borrows capture");
    agent.close();
    const terminal = agent.getSnapshot();
    agent.close();
    assert(agent.getSnapshot() === terminal, "terminal public facade cleanup");
    assert(
      closedDuringInitialization.connection === "disconnected",
      "initialization completion is fenced",
    );
    return true;
  } finally {
    agent.close();
    closedDuringInitialization.close();
    borrowed.stop();
  }
}
