import { Track } from "./platform.mjs";

export async function publicContract(pulsebeam) {
  const { createAgent, createCaptureSource, LocalTrackCapacityError } =
    pulsebeam;
  const assert = (condition, label) => {
    if (!condition) throw new Error(label);
  };
  const first = createAgent({
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
  const track = new Track();
  try {
    first.connect();
    first.disconnect();
    const camera = first.local.video("camera");
    assert(first.local.video("camera") === camera, "stable local reservation");
    assert(first.local.audio("camera") !== camera, "separate media namespaces");
    assert(camera.source === null, "empty local source");
    let exhausted = false;
    try {
      first.local.video("screen");
    } catch (error) {
      exhausted = error instanceof LocalTrackCapacityError;
    }
    assert(exhausted, "public capacity failure");
    camera.setSource(createCaptureSource(track, "video"));
    await first.replaceLocalTrack("v0", track, { contentHint: "motion" });
    assert(
      first.connection === "disconnected",
      "latest desire crosses packaged WASM initialization",
    );
    const validation = new Promise((resolve, reject) => {
      const remove = first.subscribeEvents((event) => {
        if (event.type !== "failure" || event.class !== "validation") return;
        remove();
        resolve();
      });
    });
    first.setState({
      connected: false,
      video: [
        {
          slot: 0,
          trackId: "a",
          height: 180,
          minHeight: 180,
          minFps: 15,
          priority: 100,
        },
        {
          slot: 0,
          trackId: "b",
          height: 180,
          minHeight: 180,
          minFps: 15,
          priority: 100,
        },
      ],
    });
    await validation;
    first.close();
    const terminal = first.getSnapshot();
    first.close();
    first.setState({ connected: true });
    assert(
      first.getSnapshot() === terminal && camera.source === null,
      "terminal public facade cleanup",
    );
    assert(track.readyState === "live", "public facade borrows capture");
    assert(
      closedDuringInitialization.connection === "disconnected",
      "initialization completion is fenced",
    );
    return true;
  } finally {
    first.close();
    closedDuringInitialization.close();
    track.stop();
  }
}
