(async () => {
  const wasm = await import("/dist/uniffi-proof/generated/bindings/wasm-bindgen/index.js");
  await wasm.default({
    module_or_path: "/dist/uniffi-proof/generated/bindings/wasm-bindgen/index_bg.wasm",
  });
  const core = await import("/dist/uniffi-proof/generated/bindings/pulsebeam_agent_core.js");
  const web = await import("/dist/uniffi-proof/generated/bindings/pulsebeam_agent_web.js");
  const registry = await import("/dist/uniffi-proof/web/media-registry.js");
  core.default.initialize();
  web.default.initialize();

  const canvas = document.createElement("canvas");
  const track = canvas.captureStream(5).getVideoTracks()[0];
  const trackHandle = registry.lowerMediaStreamTrack(track);
  const trackIdentity = registry.liftMediaStreamTrack(trackHandle) === track;
  let foreignRejected = false;
  try {
    registry.liftMediaStream(trackHandle);
  } catch (error) {
    foreignRejected = error instanceof ReferenceError;
  }
  registry.invalidateMediaForTest(track);
  let staleRejected = false;
  try {
    registry.liftMediaStreamTrack(trackHandle);
  } catch (error) {
    staleRejected = error instanceof ReferenceError;
  }

  const releaseCanvas = document.createElement("canvas");
  const releaseTrack = releaseCanvas.captureStream(5).getVideoTracks()[0];
  const releaseHandle = registry.lowerMediaStreamTrack(releaseTrack);
  const retainedBeforeTrackRelease = registry.size().toString();
  registry.invalidateMediaForTest(releaseTrack);
  const retainedAfterTrackRelease = registry.size().toString();

  const stream = new MediaStream();
  const streamHandle = registry.lowerMediaStream(stream);
  const streamIdentity = registry.liftMediaStream(streamHandle) === stream;
  const retainedBeforeStreamRelease = registry.size().toString();
  registry.invalidateMediaForTest(stream);
  const retainedAfterStreamRelease = registry.size().toString();

  registry.exhaustMediaHandlesForTest();
  const exhaustedCanvas = document.createElement("canvas");
  const exhaustedTrack = exhaustedCanvas.captureStream(5).getVideoTracks()[0];
  let exhaustionRejected = false;
  try {
    registry.lowerMediaStreamTrack(exhaustedTrack);
  } catch (error) {
    exhaustionRejected = error instanceof RangeError;
  }

  track.stop();
  releaseTrack.stop();
  exhaustedTrack.stop();
  return {
    trackIdentity,
    streamIdentity,
    foreignRejected,
    staleRejected,
    exhaustionRejected,
    retainedBeforeTrackRelease,
    retainedAfterTrackRelease,
    retainedBeforeStreamRelease,
    retainedAfterStreamRelease,
  };
})()
