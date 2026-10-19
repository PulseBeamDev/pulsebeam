(async () => {
  const wasm = await import(
    "/dist/uniffi-proof/generated/bindings/wasm-bindgen/index.js"
  );
  await wasm.default({
    module_or_path:
      "/dist/uniffi-proof/generated/bindings/wasm-bindgen/index_bg.wasm",
  });
  const { default: web } = await import(
    "/dist/uniffi-proof/generated/bindings/pulsebeam_agent_web.js"
  );
  const registry = await import("/dist/uniffi-proof/web/media-registry.js");
  web.initialize();
  const track = document
    .createElement("canvas")
    .captureStream(5)
    .getVideoTracks()[0];
  const stream = new MediaStream([track]);
  try {
    const tracks = web.converters.FfiConverterTypeWebMediaTrack;
    const streams = web.converters.FfiConverterTypeWebMediaStream;
    return {
      trackIdentity: tracks.lift(tracks.lower(track)) === track,
      streamIdentity: streams.lift(streams.lower(stream)) === stream,
    };
  } finally {
    registry.invalidateMediaForTest(track);
    registry.invalidateMediaForTest(stream);
    track.stop();
  }
})();
